use crate::FunctionCallError;
use crate::digest_writer::DigestWriter;
use crate::tools::command_output_artifact::RECOVERY_AGGREGATE_TOKEN_CEILING;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::ToolOutputSnapshot;
use crate::tools::command_output_artifact::find_other_thread_artifact_owner;
use crate::tools::command_output_artifact::load_tool_output_snapshot;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_LEGACY_RANGES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES;
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
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

const DEFAULT_LINE_COUNT: usize = 200;
const MAX_AGGREGATE_LINES: usize = 2_000;
const MAX_AUTOMATIC_CONTINUATION_PAGES: u32 = 64;
// A nested result is serialized into a code-mode cell and then into the outer
// exec result. Reserve enough space for that outer envelope so a fitting exact
// recovery cannot be recursively truncated into another artifact.
const CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS: usize = 1_000;
const CODE_MODE_RECOVERY_TOKEN_CEILING: usize =
    codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
        .saturating_sub(CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS);

pub(crate) use codex_tool_artifacts::recovery::DrainedRecoveryTransaction;
use codex_tool_artifacts::recovery::{recovery_envelope, recovery_call_succeeded, drain_recovery_snapshot_with_byte_limit};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputArgs {
    artifact_id: String,
    source_thread_id: Option<codex_protocol::ThreadId>,
    #[serde(default)]
    audit_only: bool,
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
    #[serde(default)]
    cell_recovery: bool,
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
}

impl ToolOutput for ReadToolOutputToolOutput {
    fn code_mode_failure_is_error(&self) -> bool {
        self.inner.code_mode_failure_is_error()
    }

    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.inner.success_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.inner.sampling_request_signal()
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
        Box::pin(async move {
            handle_read_tool_output(invocation).await.map_err(settled_recovery_failure)
        })
    }
}

impl CoreToolRuntime for ReadToolOutputHandler {
    fn cancellation_cleanup_policy(&self) -> crate::tools::registry::ToolCleanupPolicy {
        crate::tools::registry::ToolCleanupPolicy::InterruptibleRead
    }

    fn terminal_failure_reuse(&self) -> crate::tools::registry::TerminalFailureReuse {
        crate::tools::registry::TerminalFailureReuse::RequestRevisionAndJsonSyntax
    }
}

struct RecoveryAttemptObservation<'a> {
    timing: &'a codex_turn_timing::turn_timing::TurnTimingState,
    thread_id: &'a codex_protocol::ThreadId,
    turn_id: &'a str,
    call_id: &'a str,
    source_thread_id: codex_protocol::ThreadId,
    artifact_id: String,
    started: std::time::Instant,
    phase: &'static str,
    completed: bool,
    audit_only: bool,
    delivery_scope: &'static str,
    selection_observed: bool,
    selection_successful: bool,
    selection_complete: bool,
    retained_artifact_complete: bool,
}

impl Drop for RecoveryAttemptObservation<'_> {
    fn drop(&mut self) {
        if !self.audit_only {
            self.timing.record_artifact_recovery(self.call_id, self.started, serde_json::json!({
                "artifact_id":self.artifact_id, "source_thread_id":self.source_thread_id,
                "phase":self.phase, "completed":self.completed,
                "selection_observed":self.selection_observed,
                "selection_successful":self.selection_successful,
                "selection_complete":self.selection_complete,
                "retained_artifact_complete":self.retained_artifact_complete,
                "delivery_scope":self.delivery_scope, "model_delivery_verified":false,
            }));
        }
        tracing::info!(event.name = "codex.tool_output_recovery.attempt_finished",
            thread_id = %self.thread_id, turn_id = self.turn_id, call_id = self.call_id,
            source_thread_id = %self.source_thread_id, artifact_id = self.artifact_id.as_str(),
            phase = self.phase, completed = self.completed,
            delivery_scope = self.delivery_scope,
            selection_observed = self.selection_observed,
            selection_successful = self.selection_successful,
            selection_complete = self.selection_complete,
            retained_artifact_complete = self.retained_artifact_complete,
            model_delivery_verified = false,
            elapsed_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX),
            elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "output recovery attempt settled (including failed and abandoned attempts)");
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.counter("codex.tool_output_recovery.attempt", 1,
                &[("phase", self.phase), ("completed", if self.completed { "true" } else { "false" })]);
        }
    }
}

// This handler only observes retained bytes. Settled legacy read failures do
// not leave user mutations to reconcile; preserve any explicit owner contract.
fn settled_recovery_failure(error: FunctionCallError) -> FunctionCallError {
    match error {
        FunctionCallError::RespondToModel(message) => FunctionCallError::structured(
            codex_protocol::tool_failure::ToolFailure::no_effects(
                codex_protocol::tool_failure::ToolFailureCode::Failed, message,
            ).with_completed_execution(),
        ),
        other => other,
    }
}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let mut attempt = RecoveryAttemptObservation {
        timing: &invocation.step_context.turn.turn_timing_state,
        thread_id: &invocation.session.thread_id,
        turn_id: &invocation.step_context.turn.sub_id,
        call_id: &invocation.call_id,
        source_thread_id: invocation.session.thread_id,
        artifact_id: String::new(),
        started: std::time::Instant::now(), phase: "arguments", completed: false,
        audit_only: false,
        delivery_scope: "unknown",
        selection_observed: false,
        selection_successful: false,
        selection_complete: false,
        retained_artifact_complete: false,
    };
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args = parse_read_tool_output_args(arguments)?;
    attempt.audit_only = args.audit_only;
    let source_thread_id = args.source_thread_id.unwrap_or(invocation.session.thread_id);
    attempt.source_thread_id = source_thread_id;
    // Do not put arbitrary malformed argument text into diagnostic logs.
    attempt.artifact_id = args.artifact_id
        .parse::<crate::tools::command_output_artifact::ToolOutputArtifactId>()
        .ok()
        .map(|id| id.to_string())
        .filter(|id| id == &args.artifact_id)
        .unwrap_or_default();
    attempt.phase = "authorization";
    let foreign_source = source_thread_id != invocation.session.thread_id;
    if foreign_source {
        // Possession of a child-private ID is not authority. Verify persisted
        // spawn lineage, without waking or asking the producer to run again.
        let subtree = invocation.session.services.agent_control
            .list_live_agent_subtree_thread_ids(invocation.session.thread_id).await
            .map_err(|error| FunctionCallError::RespondToModel(format!("cannot authorize artifact owner: {error}")))?;
        if !subtree.contains(&source_thread_id) {
            return Err(FunctionCallError::RespondToModel("source_thread_id must name this thread or its verified descendant; peer/ancestor/unrelated artifacts remain private".into()));
        }
    }
    if args.audit_only {
        if args.selectors.is_some() || args.start_line.is_some() || args.end_line.is_some()
            || args.ranges.is_some() || args.max_bytes.is_some() || args.cell_recovery
        {
            return Err(FunctionCallError::RespondToModel(
                "audit_only cannot be combined with content selectors or delivery controls; no content was read".into(),
            ));
        }
        attempt.phase = "metadata_audit";
        let result = audit_evidence_handle(&invocation, &args.artifact_id, source_thread_id).await;
        attempt.completed = result.is_ok();
        return result;
    }
    let selectors = resolved_selectors(&args)?;
    let code_mode_recovery = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    attempt.delivery_scope = if code_mode_recovery { "code_mode" } else { "direct" };
    let delivery_policy = if code_mode_recovery {
        None
    } else {
        let policy: codex_protocol::protocol::TruncationPolicy =
            invocation.step_context.turn.model_info.truncation_policy.into();
        let limit = invocation
            .step_context
            .model_output_token_limit(&invocation.session, policy.token_budget())
            .await;
        Some(match policy {
            codex_protocol::protocol::TruncationPolicy::Bytes(bytes) => {
                codex_protocol::protocol::TruncationPolicy::Bytes(bytes.min(limit.saturating_mul(4)))
            }
            codex_protocol::protocol::TruncationPolicy::Tokens(_) => {
                codex_protocol::protocol::TruncationPolicy::Tokens(limit)
            }
        })
    };
    let max_bytes = match (code_mode_recovery, args.max_bytes) {
        (true, None) => READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
        (true, Some(bytes)) if bytes > READ_TOOL_OUTPUT_MAX_BYTES => {
            bytes.min(READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES)
        }
        _ => resolved_max_bytes(args.max_bytes)?,
    };
    // Fit the actual escaped response, rather than charging every source byte
    // the worst-case expansion. ASCII logs can use the requested script budget.
    let command_delivery_limit = if code_mode_recovery {
        READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES
    } else {
        delivery_policy.expect("direct recovery has a delivery policy").byte_budget()
    };
    let mut command_limit = max_bytes;
    let mut command_identity = None;
    // Oversized envelope of the previous attempt, and whether that attempt was
    // already the halving retry after a cut that did not shrink the envelope.
    let mut previous_envelope = usize::MAX;
    let mut halved_after_stall = false;
    attempt.phase = "command_selection";
    loop {
        let Some(mut output) = crate::tools::command_output_artifact::capture::read_command_log(
            invocation.step_context.turn.config.codex_home.as_path(),
            &source_thread_id.to_string(), &args.artifact_id,
            &selectors, command_limit,
        ).await.map_err(|error| FunctionCallError::RespondToModel(error.for_model()))? else {
            if command_identity.is_some() {
                return Err(FunctionCallError::RespondToModel(
                    "Command log disappeared while fitting recovery; no mixed page was delivered.".into(),
                ));
            }
            break;
        };
        if foreign_source { output["source_thread_id"] = serde_json::json!(source_thread_id); }
        let identity = (output["canonical_sha256"].clone(), output["canonical_bytes"].clone());
        if command_identity.as_ref().is_some_and(|previous| previous != &identity) {
            return Err(FunctionCallError::RespondToModel(
                "Command log identity changed while fitting recovery; no mixed page was delivered.".into(),
            ));
        }
        command_identity = Some(identity);
        let model_value = (!code_mode_recovery).then(|| {
            codex_code_mode::model_visible_tool_result(
                &ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME), &output,
            ).unwrap_or_else(|| output.clone())
        });
        let delivered = model_value.as_ref().unwrap_or(&output);
        let envelope_bytes = serde_json::to_vec(delivered).map_err(|error|
            FunctionCallError::RespondToModel(format!("failed to serialize command recovery: {error}"))
        )?.len();
        if envelope_bytes > command_delivery_limit {
            let has_source = output["results"].as_array().is_some_and(|results|
                results.iter().any(|result| ["text", "data_base64"].iter().any(|key|
                    result[*key].as_str().is_some_and(|text| !text.is_empty()))
                    || matches!(result["selector"]["kind"].as_str(), Some("structured" | "search")) && result["value"].is_object()));
            // Every retry rereads the log and reruns its searches/extraction.
            // A proportional cut can miss a whole row/line boundary, so halve
            // once when the envelope did not shrink; if that also fails, the
            // excess is metadata or an already minimal search/structured page.
            let shrunk = envelope_bytes < previous_envelope;
            let smaller = if shrunk {
                command_limit.saturating_mul(command_delivery_limit) / envelope_bytes.max(1)
            } else {
                command_limit / 2
            };
            let smaller = smaller.min(command_limit.saturating_sub(1));
            if !has_source
                || (!shrunk && halved_after_stall)
                || smaller == 0
                || invocation.cancellation_token.is_cancelled()
            {
                return Err(FunctionCallError::RespondToModel(
                    "Command recovery cannot fit this delivery budget; request fewer selectors or use bounded byte reads of the saved log.".into(),
                ));
            }
            halved_after_stall = !shrunk;
            previous_envelope = envelope_bytes;
            command_limit = smaller;
            continue;
        }
        let success = output["results"].as_array().is_some_and(|results|
            results.iter().all(|result| result["status"] == "ok"));
        // Plain command logs bypass the snapshot path below. Completed native
        // selections must still discharge this artifact's recovery obligations.
        // Never treat a partial selection or merely retained bytes as delivery.
        let evidence = (success && output["complete"] == true
            && output["retained_artifact_complete"] == true)
            .then(|| output["results"].as_array())
            .flatten()
            .map(|results| results.iter().filter_map(|result| {
                if !result["text"].is_string() && !result["data_base64"].is_string() {
                    return None;
                }
                let start = result["canonical_range"]["start"].as_u64()?;
                let end = result["canonical_range"]["end"].as_u64()?;
                (start <= end && end <= output["canonical_bytes"].as_u64()?)
                    .then(|| serde_json::json!([start, end]))
            }).collect::<Vec<_>>())
            .filter(|ranges| !ranges.is_empty())
            .map(|ranges| serde_json::json!({
                "source": "artifact", "scope": output["artifact_id"],
                "identity": {"sha256": output["canonical_sha256"], "ranges": ranges},
            }));
        attempt.selection_observed = true;
        attempt.selection_successful = success;
        attempt.selection_complete = output["complete"] == true;
        attempt.retained_artifact_complete = output["retained_artifact_complete"] == true;
        attempt.phase = "command_delivery";
        let mut result = JsonToolOutput::with_success(output, Some(success))
            .with_code_mode_failure_as_data();
        if let Some(evidence) = evidence {
            let mut signal = crate::tools::context::semantic_evidence_sampling_signal(evidence);
            if let Some(receipt) = super::read_file::native_read_measurement(result.value(), &serde_json::json!({
                "artifact":args.artifact_id, "thread":source_thread_id,
            })) { signal["native_read"] = receipt; }
            result = result.with_sampling_request_signal(signal);
        }
        if let Some(model_value) = model_value {
            result = result.with_model_value(model_value);
        }
        crate::tools::context::ensure_exact_model_delivery(&result, &invocation)?;
        record_recovery_metrics(&invocation);
        // Command logs return before the snapshot recovery path below. Honor
        // the same opt-in cell allowance without relaxing its hard ceiling.
        if args.cell_recovery
            && let ToolCallSource::CodeMode { cell_id, .. } = &invocation.source
        {
            let tokens = codex_utils_output_truncation::model_token_count(&result.value().to_string());
            invocation.session.services.code_mode_service.raise_output_budget(cell_id, tokens);
        }
        attempt.completed = true;
        return Ok(boxed_tool_output(result));
    }
    let mut action_bounds_digest = Sha256::new();
    serde_json::to_writer(DigestWriter(&mut action_bounds_digest), &selectors).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery selectors: {err}"))
    })?;
    let action_bounds_hash = hex::encode(action_bounds_digest.finalize());
    // JavaScript consumes exact data before choosing what to print. Coupling
    // this selection to its display budget makes a small summary require extra
    // recovery calls. Scripts automatically drain within the fixed payload cap;
    // direct reads and explicit smaller budgets retain display-sized caps.
    let token_ceiling = if code_mode_recovery && max_bytes > READ_TOOL_OUTPUT_MAX_BYTES {
        // Approximate token costs use UTF-8 bytes / 4. Leave ample space for
        // escaping, continuation metadata and the retry-avoidance margin.
        (READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES - 128 * 1024) / 4
    } else if code_mode_recovery {
        CODE_MODE_RECOVERY_TOKEN_CEILING
    } else {
        RECOVERY_AGGREGATE_TOKEN_CEILING.min(
            delivery_policy.expect("direct recovery has a delivery policy").token_budget(),
        )
    };
    attempt.phase = "snapshot_load";
    let snapshot = match load_tool_output_snapshot(
        invocation.step_context.turn.config.codex_home.as_path(),
        &source_thread_id.to_string(),
        &args.artifact_id,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(ReadToolOutputError::Expired) if !foreign_source => {
            let error = missing_artifact_error(&invocation, &args.artifact_id).await;
            return Err(FunctionCallError::RespondToModel(error.for_model()));
        }
        Err(err) => return Err(FunctionCallError::RespondToModel(err.for_model())),
    };
    // The persisted counter keeps its "reread" name but has artifact-read
    // semantics: one per call that loaded the artifact snapshot, whether or not
    // its selections succeed or repeat earlier deliveries. Prior delivery is
    // tracked only by the session's tool-history recovery ledger.
    invocation.step_context.turn.turn_timing_state.record_tool_output_artifact_reread();
    attempt.phase = "selection";
    let transaction = drain_recovery_snapshot_for_delivery(
        &snapshot,
        selectors,
        token_ceiling,
        max_bytes,
        &invocation.cancellation_token,
        delivery_policy,
        foreign_source.then_some(source_thread_id),
    )
    .await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    let DrainedRecoveryTransaction {
        output,
        drained_continuation_pages,
        continuation_stop,
    } = transaction;
    // Typed overflow reports an incomplete selection without losing retained
    // evidence. Recovery bypasses recursive spilling, and a direct recovery is
    // exempt from generic re-projection, so this delivery is never truncated
    // again. An in-cell recovery reaches the model only through its cell's
    // output budget; that delivery attributes any re-truncation.
    // Direct reads and in-cell transactions have separate timing contracts.
    // Only direct recovery marks a following provider tool-result generation.
    // Automatic continuation pages are tracked separately by the transaction.
    record_recovery_metrics(&invocation);

    let exact_recovery_receipt = exact_code_mode_recovery_receipt(
        code_mode_recovery,
        &output,
        action_bounds_hash,
        drained_continuation_pages,
    );
    let successful = recovery_call_succeeded(&output, continuation_stop.as_ref());
    attempt.selection_observed = true;
    attempt.selection_successful = successful;
    attempt.selection_complete = output.complete;
    attempt.retained_artifact_complete = output.capture_complete;
    let evidence = output.delivered_evidence().map(|identity| {
        serde_json::json!({
            "source": "artifact",
            "scope": output.artifact_id,
            "identity": identity,
        })
    });
    let mut selectors = output.delivered_ranges().into_iter().map(|(start, end)|
        serde_json::json!({"kind":"bytes", "start":start, "end":end, "source_inspection":false})
    ).collect::<Vec<_>>();
    let source_bytes = output.canonical_bytes;
    let source_sha256 = output.canonical_sha256.clone();
    let mut output = recovery_envelope(&output, continuation_stop.as_ref()).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery result: {err}"))
    })?;
    output["historical_authenticity"] = serde_json::json!(snapshot.historical_authenticity());
    if foreign_source { output["source_thread_id"] = serde_json::json!(source_thread_id); }
    let inspection_delivery = successful
        && !foreign_source
        && snapshot.historical_authenticity() == "authenticated"
        && !invocation.cancellation_token.is_cancelled();
    let acquired = if inspection_delivery {
        super::read_file::source_inspection_selectors(&output)
    } else { Vec::new() };
    let acquired_source_bytes = !acquired.is_empty();
    selectors.extend(acquired);
    let mut inner = recovery_tool_output(output, successful, evidence.clone());
    attempt.phase = "delivery";
    crate::tools::context::ensure_exact_model_delivery(&inner, &invocation)?;
    if evidence.is_some() && !foreign_source {
        invocation.session.record_tool_history_recovery(
            args.artifact_id.clone(), invocation.call_id.clone(), selectors,
        ).await;
    }
    if acquired_source_bytes {
        let proof = super::read_file::completed_source_inspection_evidence(
            &invocation, &args.artifact_id, source_bytes, &source_sha256,
        ).await;
        if !matches!(&proof, Ok(None)) {
            let mut output = inner.value().clone();
            let mut projected = codex_code_mode::model_visible_tool_result(
                &ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME), &output,
            ).unwrap_or_else(|| output.clone());
            super::read_file::attach_source_inspection_evidence(&mut output, &mut projected, proof, &invocation);
            inner = recovery_tool_output(output, successful, evidence).with_model_value(projected);
        }
    }
    // A truncated cell's recovery.arguments read its omitted pages in one call;
    // give this delivery room beyond the cell budget so it prints whole.
    if args.cell_recovery
        && let ToolCallSource::CodeMode { cell_id, .. } = &invocation.source
    {
        let tokens = codex_utils_output_truncation::model_token_count(&inner.value().to_string());
        invocation
            .session
            .services
            .code_mode_service
            .raise_output_budget(cell_id, tokens);
    }
    attempt.completed = true;
    Ok(boxed_tool_output(ReadToolOutputToolOutput {
        inner,
        exact_recovery: exact_recovery_receipt,
    }))
}

fn record_recovery_metrics(invocation: &ToolInvocation) {
    let in_cell = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_tool_output_recovery(in_cell, /*retruncation_count*/ 0);
    if let ToolCallSource::CodeMode { cell_id, .. } = &invocation.source {
        invocation
            .session
            .services
            .code_mode_service
            .record_packet_tool_output_recovery(cell_id);
    }
}

/// Audit through the same owners as recovery. This is a point-in-time access
/// observation, not content delivery, semantic evidence or a retention lease.
async fn audit_evidence_handle(
    invocation: &ToolInvocation,
    artifact_id: &str,
    source_thread_id: codex_protocol::ThreadId,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let home = invocation.step_context.turn.config.codex_home.as_path();
    let thread_id = source_thread_id.to_string();
    let (mut audit, command_log) =
        crate::tools::command_output_artifact::audit_tool_output_reference(
            home, &thread_id, artifact_id, READ_TOOL_OUTPUT_MAX_BYTES,
        ).await;
    let readable = audit["access"] == "readable";
    let directory = home.join(if command_log { "command-output" } else { "tool-output" }).join(&thread_id);
    audit["retention"]["pressure"] = crate::tools::command_output_artifact::pressure::audit(
        directory, artifact_id.to_string(),
    ).await;
    if source_thread_id == invocation.session.thread_id {
        let history = invocation.session.clone_history().await;
        audit["retention"]["recovery_obligation"] = history.tool_history_state_ref()
            .retention_recovery_obligation(artifact_id);
    } else if audit["recovery"]["arguments"].is_object() {
        audit["recovery"]["arguments"]["source_thread_id"] = serde_json::json!(source_thread_id);
    }
    let output = JsonToolOutput::with_success(audit, Some(readable)).with_code_mode_failure_as_data();
    crate::tools::context::ensure_exact_model_delivery(&output, invocation)?;
    Ok(boxed_tool_output(output))
}

// Ownership diagnosis must not turn a failed lookup into a long wait.
const ARTIFACT_OWNER_RELATION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// A handle missing from this thread may still be retained by the thread that
/// produced it, such as a subagent. Name that owner instead of reporting
/// expiry; recovery itself stays confined to the calling thread.
async fn missing_artifact_error(
    invocation: &ToolInvocation,
    artifact_id: &str,
) -> ReadToolOutputError {
    let Some(owner_thread_id) = find_other_thread_artifact_owner(
        invocation.step_context.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        artifact_id,
    )
    .await
    else {
        return ReadToolOutputError::Expired;
    };
    // Report only positive membership: retained spawn edges cannot certify a
    // complete subtree, and an unavailable or slow graph is not evidence.
    let owner = codex_protocol::ThreadId::from_string(&owner_thread_id);
    let known_descendant = tokio::time::timeout(
        ARTIFACT_OWNER_RELATION_TIMEOUT,
        invocation
            .session
            .services
            .agent_control
            .list_live_agent_subtree_thread_ids(invocation.session.thread_id),
    )
    .await
    .is_ok_and(|subtree| {
        subtree.is_ok_and(|subtree| owner.as_ref().is_ok_and(|owner| subtree.contains(owner)))
    });
    ReadToolOutputError::OwnedByOtherThread {
        owner_thread_id,
        known_descendant,
    }
}

// Pagination is a successful bounded read, not a terminal selection failure.
// Keep selection completeness and the continuation owner's remaining selectors
// intact; cancellation, missing bytes and invalid siblings still fail the call.
fn recovery_tool_output(
    output: Value,
    successful: bool,
    evidence: Option<Value>,
) -> JsonToolOutput {
    let projected = codex_code_mode::model_visible_tool_result(
        &ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME),
        &output,
    );
    let mut result =
        JsonToolOutput::with_success(output, Some(successful)).with_code_mode_failure_as_data();
    if let Some(projected) = projected {
        result = result.with_model_value(projected);
    }
    if successful {
        if let Some(evidence) = evidence {
            result = result.with_sampling_request_signal(
                crate::tools::context::semantic_evidence_sampling_signal(evidence),
            );
        }
    } else {
        let errors = result.value()["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|result| result["status"] != "ok" || result["complete"] == false)
            .map(|result| {
                serde_json::json!({
                    "selector": result["selector"], "status": result["status"],
                    "complete": result["complete"], "message": result["message"],
                })
            })
            .collect::<Vec<_>>();
        let mut signal =
            crate::tools::context::semantic_failure_sampling_signal(serde_json::json!({
                "artifact": result.value()["canonical_sha256"],
                "errors": errors,
                "continuation_stop": result.value()["continuation_stop"],
            }));
        // Partial recovery has two independent facts: delivered source bytes
        // and unresolved selections. Neither may erase the other.
        if let Some(signal) = signal.as_object_mut() {
            signal.remove("semantic_evidence");
            if let Some(evidence) = evidence {
                signal.insert("semantic_evidence".to_string(), evidence);
            }
        }
        result = result.with_sampling_request_signal(signal);
    }
    if let Some(receipt) = super::read_file::native_read_measurement(result.value(), &serde_json::json!({
        "artifact":result.value()["artifact_id"], "thread":result.value()["source_thread_id"],
    })) {
        let mut signal = result.sampling_request_signal().unwrap_or_else(|| serde_json::json!({}));
        signal["native_read"] = receipt;
        result = result.with_sampling_request_signal(signal);
    }
    result
}

fn parse_read_tool_output_args(arguments: &str) -> Result<ReadToolOutputArgs, FunctionCallError> {
    parse_arguments(arguments).map_err(|err| match err {
        FunctionCallError::Structured(mut failure) => {
            let detail = failure
                .message
                .strip_prefix("failed to parse function arguments: ")
                .unwrap_or(&failure.message);
            let detail = file_only_selector_hint(arguments).unwrap_or_else(|| detail.to_string());
            failure.message = format!(
                "failed to parse read_tool_output arguments: {detail}. Consult the advertised read_tool_output schema."
            );
            FunctionCallError::Structured(failure)
        }
        err => err,
    })
}

/// Shared by the handler and code-mode argument preflight, which otherwise
/// reports only the schema's accepted selector kinds.
pub(crate) fn file_only_selector_hint(arguments: &str) -> Option<String> {
    file_only_selector_kind(arguments).map(|kind| format!(
        "`{kind}` selectors are read_file-only; snapshots take bytes, lines, tail, section, json_pointer, structured or search selectors. On a Rust/Python or Markdown snapshot saved with an outline, select {{\"kind\":\"section\",\"id\":\"outline\"}} for item IDs, then {{\"kind\":\"section\",\"id\":\"<item id>\"}}; otherwise search or select lines here, or use read_file on the current file"
    ))
}

/// read_file resolves names and lines through its outline parser, which needs
/// the file path; a snapshot retains only that parser's section IDs. Name the
/// equivalent section selector instead of an unknown-variant error.
fn file_only_selector_kind(arguments: &str) -> Option<&'static str> {
    let arguments: Value = serde_json::from_str(arguments).ok()?;
    arguments["selectors"]
        .as_array()?
        .iter()
        .find_map(|selector| match selector["kind"].as_str()? {
            "symbol" => Some("symbol"),
            "enclosing" => Some("enclosing"),
            _ => None,
        })
}



async fn drain_recovery_snapshot_for_delivery(
    snapshot: &std::sync::Arc<ToolOutputSnapshot>,
    selectors: Vec<ToolOutputSelector>,
    mut token_ceiling: usize,
    max_bytes: usize,
    cancellation_token: &CancellationToken,
    policy: Option<codex_protocol::protocol::TruncationPolicy>,
    source_thread_id: Option<codex_protocol::ThreadId>,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
    loop {
        let result = drain_recovery_snapshot_with_byte_limit(
            snapshot,
            selectors.clone(),
            token_ceiling,
            max_bytes,
            cancellation_token,
            READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
            MAX_AUTOMATIC_CONTINUATION_PAGES,
        )
        .await?;
        let Some(policy) = policy else {
            return Ok(result);
        };
        let mut envelope = recovery_envelope(&result.output, result.continuation_stop.as_ref())
            .map_err(|error| ReadToolOutputError::Io(error.to_string()))?;
        // This field is always present in the final direct result. Omitting it
        // here lets a page fill the budget and then fail the exact-delivery guard.
        envelope["historical_authenticity"] = serde_json::json!(snapshot.historical_authenticity());
        // Descendant recovery adds its owner to the final packet too. Fit the
        // same envelope that the exact-delivery guard will receive below.
        if let Some(source_thread_id) = source_thread_id {
            envelope["source_thread_id"] = serde_json::json!(source_thread_id);
        }
        let projected = codex_code_mode::model_visible_tool_result(
            &ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME),
            &envelope,
        )
        .unwrap_or(envelope);
        let bytes = serde_json::to_vec(&projected)
            .map_err(|error| ReadToolOutputError::Io(error.to_string()))?
            .len();
        // Check the final escaped model view in bytes. Byte policies must not
        // round up to tokens, and the drain's retry margin is inside this cap.
        if bytes <= policy.byte_budget() {
            return Ok(result);
        }
        let margin = crate::tools::command_output_artifact::RECOVERY_RETRY_AVOIDANCE_TOKEN_MARGIN;
        let smaller = token_ceiling
            .saturating_add(margin)
            .saturating_mul(policy.byte_budget())
            / bytes.max(1);
        let smaller = smaller
            .saturating_sub(margin)
            .min(token_ceiling.saturating_sub(1));
        if smaller == 0 || cancellation_token.is_cancelled() {
            return Err(ReadToolOutputError::InvalidRange(
                "Recovery metadata cannot fit the current delivery budget; request fewer or narrower selectors.".into(),
            ));
        }
        token_ceiling = smaller;
    }
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
        Some(0) => Err(FunctionCallError::RespondToModel(format!(
            "max_bytes must be between 1 and {READ_TOOL_OUTPUT_MAX_BYTES}"
        ))),
        Some(max_bytes) => Ok(max_bytes.min(READ_TOOL_OUTPUT_MAX_BYTES)),
        None => Ok(READ_TOOL_OUTPUT_MAX_BYTES),
    }
}

