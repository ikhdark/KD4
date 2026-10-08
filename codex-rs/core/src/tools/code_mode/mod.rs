mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod response_adapter;
mod wait_handler;
pub(crate) mod wait_spec;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::NestedCancellation;
use codex_code_mode::RuntimeResponse;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::OnceCell;

use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_execution::CodeModeToolResult;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::RequiredToolTerminalCause;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::effective_tool_mode;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::parallel::required_tool_error_terminal_cause;
use crate::tools::parallel::required_tool_terminal_cause;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::can_request_original_image_detail;
use codex_tools::sanitize_original_image_detail as sanitize_image_detail_items;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::resolve_output_limits;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;

use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use response_adapter::into_function_call_output_content_items;
pub(crate) use wait_handler::CodeModeWaitHandler;
pub(crate) use wait_handler::NESTED_DEFAULT_POLL;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
const FAILED_CELL_ITEM_NAMESPACE: &str = "codex.internal";
const FAILED_CELL_ITEM_TOOL: &str = "code_mode_cell";
/// Essential-inline marker: the model-visible packet omits output that the
/// canonical artifact retains, so admission must name that artifact.
pub(crate) const VISIBLE_OUTPUT_TRUNCATED_KEY: &str = "visible_output_truncated";

/// Process exit is not output completion: a terminal command can still own a
/// drainable pipe and must keep its handle in rendering and history admission.
pub(crate) fn command_owns_work(state: &JsonValue) -> bool {
    state["session_id"].as_u64().is_some()
        && (state["process_exited"] != true || state["output_complete"] == false)
}

/// Returns true for the un-namespaced code-mode `exec` tool.
pub(crate) fn is_exec_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none() && tool_name.name == PUBLIC_TOOL_NAME
}

/// Both entry points orchestrate nested tools, which own their workspace gates
/// and output projection independently of the outer code-mode packet.
pub(crate) fn is_orchestration_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none()
        && matches!(tool_name.name.as_str(), PUBLIC_TOOL_NAME | WAIT_TOOL_NAME)
}

#[derive(Clone)]
pub(crate) struct ExecContext {
    pub(super) session: Arc<Session>,
    pub(super) turn: Arc<TurnContext>,
}

pub(crate) struct CodeModeService {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    recovery_path: Option<std::path::PathBuf>,
    session_provider: Arc<dyn CodeModeSessionProvider>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    packet_admission: Mutex<CodeModePacketAdmission>,
    cell_parent_call_ids: Mutex<HashMap<String, String>>,
    trace_tails: Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct CodeModePacketAdmission {
    cells: HashMap<String, CodeModePacketMetrics>,
    // Closed cells release execution state, not unresolved evidence. Keep only
    // exact recovery descriptors; the artifact owner retains the actual bytes.
    closed_recovery: HashMap<String, Vec<HashMap<String, DeliveryArtifactCoverage>>>,
}

#[derive(Default)]
struct CodeModePacketMetrics {
    turn_id: Option<String>,
    output_budget: Option<usize>,
    delivery_intent: Option<CodeModeDeliveryIntent>,
    delivery_blocked: bool,
    delivery_recovery: HashMap<String, DeliveryArtifactCoverage>,
    command_states: Vec<JsonValue>,
    next_nested_ordinal: usize,
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    nested_recovery: std::collections::BTreeMap<usize, JsonValue>,
    omitted_nested_result_count: usize,
    first_required_terminal: Option<CodeModeNestedTerminal>,
}

struct CodeModeDeliveryIntent {
    turn_id: String,
    input_activity: tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
    schema: Option<JsonValue>,
    limit: usize,
}

#[derive(Default)]
struct DeliveryArtifactCoverage {
    sha256: String,
    bytes: u64,
    ranges: Vec<(u64, u64)>,
    // Empty means the whole canonical artifact. Otherwise only these requested
    // selections are required; unrelated source bytes are not an obligation.
    required_ranges: Vec<(u64, u64)>,
}

impl DeliveryArtifactCoverage {
    fn recovered(&self) -> bool {
        if self.required_ranges.is_empty() {
            self.bytes == 0 || self.ranges.first().is_some_and(|range| *range == (0, self.bytes))
        } else {
            self.required_ranges.iter().all(|(start, end)| start == end
                || self.ranges.iter().any(|range| range.0 <= *start && *end <= range.1))
        }
    }

    fn observe(&mut self, evidence: &JsonValue) {
        if evidence["identity"]["sha256"].as_str() != Some(self.sha256.as_str()) { return; }
        if let Some(ranges) = evidence["identity"]["ranges"].as_array() {
            for range in ranges {
                if let (Some(start), Some(end)) = (range[0].as_u64(), range[1].as_u64())
                    && start <= end && end <= self.bytes
                {
                    self.ranges.push((start, end));
                }
            }
        }
        // A successful root JSON selection is also the entire canonical value.
        if evidence["identity"]["values"].as_array().is_some_and(|values| values.iter().any(|value|
            value["selector"]["kind"] == "json_pointer"
                && value["selector"]["pointer"] == "" && value.get("value").is_some()))
        {
            self.ranges.push((0, self.bytes));
        }
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut() && start <= last.1 {
                last.1 = last.1.max(end);
            } else { merged.push((start, end)); }
        }
        self.ranges = merged;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodeModeNestedTerminal {
    ordinal: usize,
    cause: RequiredToolTerminalCause,
    message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CodeModeNestedResultEvidence {
    failed: bool,
    #[serde(skip)]
    command_state: Option<JsonValue>,
    #[serde(skip)]
    output_fingerprints: Vec<(String, usize, [u8; 32])>,
    #[serde(skip)]
    ordinal: usize,
    call_id: String,
    parent_call_id: Option<String>,
    parent_cell_id: String,
    runtime_tool_call_id: String,
    tool_name: String,
    output: String,
    output_truncated: bool,
}

struct CodeModePacketReceipt {
    command_states: Vec<JsonValue>,
    nested_call_count: usize,
    batchable_observation_count: usize,
    result_bytes: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    nested_recovery: std::collections::BTreeMap<usize, JsonValue>,
    omitted_nested_result_count: usize,
    first_required_terminal: Option<CodeModeNestedTerminal>,
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    total_bytes: usize,
    limit: usize,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(128)),
            total_bytes: 0,
            limit,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "Bytes are validated and truncated to the valid UTF-8 prefix immediately before conversion"
    )]
    fn finish(self) -> (String, bool, usize) {
        let mut bytes = self.bytes;
        let mut truncated = self.total_bytes > bytes.len();
        if let Err(error) = std::str::from_utf8(&bytes) {
            bytes.truncate(error.valid_up_to());
            truncated = true;
        }
        (
            String::from_utf8(bytes).expect("validated UTF-8 prefix"),
            truncated,
            self.total_bytes,
        )
    }
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.total_bytes = self.total_bytes.saturating_add(buffer.len());
        let remaining = self.limit.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&buffer[..buffer.len().min(remaining)]);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_serialized_json(value: &JsonValue) -> (String, bool, usize) {
    let mut writer = BoundedJsonWriter::new(MAX_RETAINED_NESTED_RESULT_BYTES);
    if serde_json::to_writer(&mut writer, value).is_err() {
        return (
            "<nested result could not be serialized>".to_string(),
            false,
            0,
        );
    }
    writer.finish()
}

const MAX_RETAINED_NESTED_RESULTS: usize = 2;
const MAX_RETAINED_NESTED_RESULT_BYTES: usize = 1_024;
const MAX_FAILED_CELL_ERROR_BYTES: usize = 4_096;
const FAILED_CELL_ERROR_TRUNCATION_MARKER: &str = "\n… [truncated]";
const APPLY_PATCH_ENVELOPE_MARKER: &str = "*** Begin Patch";

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new());
        Self {
            session: OnceCell::new(),
            recovery_path: None,
            session_provider,
            dispatch_broker,
            packet_admission: Mutex::new(CodeModePacketAdmission::default()),
            cell_parent_call_ids: Mutex::new(HashMap::new()),
            trace_tails: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_recovery_path(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.recovery_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    pub(crate) async fn execute(
        &self,
        request: codex_code_mode::ExecuteRequest,
    ) -> Result<codex_code_mode::StartedCell, String> {
        self.session().await?.execute(request).await
    }

    pub(crate) async fn wait(
        &self,
        mut request: codex_code_mode::WaitRequest,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        request.recovery = self.recovery_path.clone().map(|path| codex_code_mode::ReceiptRecovery {
            path, terminal_only: false,
        });
        self.session().await?.wait(request).await
    }

    pub(crate) async fn wait_for_decision(
        &self,
        cell_id: codex_code_mode::CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.wait(codex_code_mode::WaitRequest {
            recovery: None,
            cell_id,
            yield_time_ms: codex_code_mode::OWNER_HELD_DECISION_YIELD_TIME_MS,
        })
        .await
    }

    /// Starts the code-mode host before the first turn needs it so the first
    /// cell does not pay for isolate and host startup.
    pub(crate) async fn prewarm(&self) -> Result<(), String> {
        self.session().await.map(|_| ())
    }

    #[cfg(test)]
    fn is_initialized(&self) -> bool {
        self.session.initialized()
    }

    pub(crate) async fn terminate(
        &self,
        cell_id: CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.terminate(cell_id).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutting_down.store(true, Ordering::Release);
        // Join any initialization already in progress without initializing an unused service.
        match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => session.shutdown().await,
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn mark_cell_ready_for_dispatch(&self, cell_id: &codex_code_mode::CellId) {
        self.dispatch_broker.mark_cell_ready_for_dispatch(cell_id);
    }

    pub(crate) fn record_cell_parent_call_id(&self, cell_id: &CellId, call_id: &str) {
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .entry(cell_id.to_string())
            .or_default();
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cell_id.to_string(), call_id.to_string());
    }

    pub(crate) fn record_output_budget(&self, cell_id: &CellId, budget: Option<usize>) {
        if let Some(metrics) = self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells.get_mut(cell_id.as_str())
            && budget.is_some()
        {
            metrics.output_budget = budget;
        }
    }

    pub(super) fn record_cell_turn(&self, cell_id: &CellId, turn_id: &str) {
        if let Some(metrics) = self.packet_admission.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).cells.get_mut(cell_id.as_str())
        {
            metrics.turn_id = Some(turn_id.to_string());
        }
    }

    pub(crate) fn output_budget(&self, cell_id: &str) -> Option<usize> {
        self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells.get(cell_id)
            .and_then(|metrics| metrics.output_budget)
    }

    pub(super) fn record_delivery_intent(
        &self,
        cell_id: &CellId,
        turn: &TurnContext,
        input_activity: tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
    ) {
        if let Some(metrics) = self.packet_admission
            .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells.get_mut(cell_id.as_str())
        {
            metrics.delivery_intent = Some(CodeModeDeliveryIntent {
                turn_id: turn.sub_id.clone(),
                input_activity,
                schema: turn.final_output_json_schema.clone(),
                limit: metrics.output_budget.unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL),
            });
            metrics.turn_id = Some(turn.sub_id.clone());
        }
    }

    pub(super) fn delivery_for_response(
        &self,
        cell_id: &CellId,
        turn: &TurnContext,
        response: &RuntimeResponse,
    ) -> Result<Option<String>, JsonValue> {
        let mut admission = self.packet_admission
            .lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior_recovery_pending = admission.closed_recovery.get(&turn.sub_id)
            .is_some_and(|cells| !cells.is_empty());
        let Some(metrics) = admission.cells.get_mut(cell_id.as_str()) else { return Ok(None); };
        if let RuntimeResponse::Yielded { content_items, .. }
            | RuntimeResponse::ExplicitYield { content_items, .. } = response
        {
            // A partial visible answer cannot safely be delivered again, or
            // reconstructed from an incomplete tail. Empty yields retain intent.
            let requested = metrics.delivery_intent.is_some();
            if !content_items.is_empty() {
                metrics.delivery_intent = None;
            }
            return if requested { Err(serde_json::json!({"category":if content_items.is_empty() {
                "cell_running" } else { "partial_output" }, "cell_id":cell_id.as_str()})) } else { Ok(None) };
        }
        let Some(intent) = metrics.delivery_intent.take() else { return Ok(None); };
        let refusal = if intent.turn_id != turn.sub_id { Some("turn_changed") }
            else if intent.schema != turn.final_output_json_schema { Some("schema_changed") }
            else if intent.input_activity.has_changed().unwrap_or(true) { Some("input_changed") }
            else if metrics.delivery_blocked { Some("nested_work_failed_or_incomplete") }
            else if prior_recovery_pending || metrics.delivery_recovery.values().any(|coverage| !coverage.recovered()) {
                Some("evidence_recovery_pending")
            }
            else if metrics.first_required_terminal.is_some() { Some("required_tool_failed") }
            else { None };
        if let Some(category) = refusal { return Err(serde_json::json!({"category":category})); }
        if let Some(state) = metrics.command_states.iter().find(|state| {
                // Absence of a running flag is not terminal proof. The command
                // owner must settle execution and deferred work affirmatively.
                state.get("execution_state").and_then(JsonValue::as_str) != Some("exited")
                    || state.get("session_id").is_some_and(|id| !id.is_null())
                    || state.get("process_exited").and_then(JsonValue::as_bool) != Some(true)
                    || !(state.get("exit_code").and_then(JsonValue::as_i64) == Some(0)
                        || (state["search_no_match"] == true && state["exit_code"] == 1))
                    || state.get("error").is_some_and(|error| !error.is_null())
                    || state.get("pending_deferred_completions")
                        .is_some_and(|pending| !pending.as_array().is_some_and(Vec::is_empty))
            })
        {
            let category = if state.get("pending_deferred_completions")
                .is_some_and(|pending| !pending.as_array().is_some_and(Vec::is_empty)) {
                "deferred_work_pending"
            } else if state["execution_state"] == "running" || state["session_id"].is_number() {
                "command_running_or_draining"
            } else { "command_failed_or_unverified" };
            return Err(serde_json::json!({"category":category, "session_id":state["session_id"],
                "execution_state":state["execution_state"], "exit_code":state["exit_code"]}));
        }
        execute_handler::schema_validated_delivery(
            response,
            intent.limit.min(metrics.output_budget.unwrap_or(intent.limit)),
            intent.schema.as_ref(),
        ).map(Some)
    }

    pub(crate) fn cell_parent_call_id(&self, cell_id: &CellId) -> Option<String> {
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id.as_str())
            .cloned()
    }

    fn record_delivery_evidence(
        &self,
        cell_id: &CellId,
        artifact_required: bool,
        artifact: Option<(String, String, u64)>,
        signal: Option<&JsonValue>,
        value: &JsonValue,
    ) -> Option<JsonValue> {
        let mut admission = self.packet_admission.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metrics) = admission.cells.get_mut(cell_id.as_str()) else { return None; };
        let mut recovery = None;
        if artifact_required {
            if let Some((id, sha256, bytes)) = artifact {
                let text = value.as_str().or_else(|| value["output"].as_str());
                let parsed = text.and_then(|text| serde_json::from_str::<JsonValue>(text).ok());
                let exact = text.is_some_and(|text| format!("{:x}", Sha256::digest(text.as_bytes())) == sha256)
                    || serde_json::to_vec(parsed.as_ref().unwrap_or(value)).is_ok_and(|bytes|
                        format!("{:x}", Sha256::digest(bytes)) == sha256);
                let coverage = metrics.delivery_recovery.entry(id.clone()).or_insert_with(||
                    DeliveryArtifactCoverage { sha256, bytes, ranges: Vec::new(), required_ranges: Vec::new() });
                // An originating canonical-output obligation covers the whole
                // artifact, even if earlier calls requested only a subset.
                coverage.required_ranges.clear();
                if exact { coverage.ranges = vec![(0, coverage.bytes)]; }
                if !coverage.recovered() {
                    recovery = Some(serde_json::json!({
                        "artifact_id": id, "canonical_sha256": coverage.sha256,
                        "canonical_bytes": coverage.bytes, "tool": "read_tool_output",
                        "arguments": {"artifact_id": id, "selectors": [
                            {"kind": "bytes", "start": 0, "end": coverage.bytes}
                        ]},
                    }));
                }
            } else {
                // Without authenticated identity, a later arbitrary read cannot
                // prove this missing canonical output was recovered.
                metrics.delivery_blocked = true;
            }
        }
        if value["complete"] == false
            && let (Some(id), Some(sha256), Some(bytes), Some(results)) = (
                value["artifact_id"].as_str(), value["canonical_sha256"].as_str(),
                value["canonical_bytes"].as_u64(), value["results"].as_array(),
            )
        {
            let required_ranges = results.iter().filter_map(|result| {
                let start = result["canonical_range"]["start"].as_u64()?;
                let end = result["canonical_range"]["end"].as_u64()?;
                (start <= end && end <= bytes).then_some((start, end))
            }).collect::<Vec<_>>();
            if required_ranges.is_empty() {
                // A cursor without authenticated ranges is not proof of exact
                // coverage. Do not make an incomplete search deliverable.
                metrics.delivery_blocked = true;
            } else {
                match metrics.delivery_recovery.entry(id.to_string()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(DeliveryArtifactCoverage {
                            sha256: sha256.to_string(), bytes, ranges: Vec::new(), required_ranges,
                        });
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if !entry.get().required_ranges.is_empty() {
                            entry.get_mut().required_ranges.extend(required_ranges);
                        }
                    }
                }
            }
        }
        if let Some(evidence) = signal.and_then(|signal| signal.get("semantic_evidence"))
            && evidence["source"] == "artifact"
            && let Some(id) = evidence["scope"].as_str()
            && let Some(coverage) = metrics.delivery_recovery.get_mut(id)
        {
            coverage.observe(evidence);
        }
        // Keep only unresolved ownership. Exact data and resolved recovery stay
        // in the existing evidence/history owners, not a growing cell ledger.
        metrics.delivery_recovery.retain(|_, coverage| !coverage.recovered());
        if let Some(evidence) = signal.and_then(|signal| signal.get("semantic_evidence"))
            && evidence["source"] == "artifact"
            && let Some(id) = evidence["scope"].as_str()
        {
            admission.closed_recovery.retain(|_, cells| {
                cells.retain_mut(|coverage| {
                    if let Some(required) = coverage.get_mut(id) {
                        required.observe(evidence);
                        if required.recovered() { coverage.remove(id); }
                    }
                    !coverage.is_empty()
                });
                !cells.is_empty()
            });
        }
        recovery
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
        self.trace_tails
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(cell_id.as_str());
        // Only registration creates packet state. Accepted child operations
        // keep their own cleanup owner, but cannot recreate a closed packet.
        {
            let mut admission = self.packet_admission.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(metrics) = admission.cells.remove(cell_id.as_str())
                && !metrics.delivery_recovery.is_empty()
                && let Some(turn_id) = metrics.turn_id
            {
                admission.closed_recovery.entry(turn_id).or_default().push(metrics.delivery_recovery);
            }
        }
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(cell_id.as_str());
    }

    fn begin_packet_call(&self, cell_id: &CellId) -> Option<usize> {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.cells.get_mut(cell_id.as_str())?;
        let ordinal = metrics.next_nested_ordinal;
        metrics.next_nested_ordinal = metrics.next_nested_ordinal.saturating_add(1);
        metrics.nested_call_count = metrics.nested_call_count.saturating_add(1);
        Some(ordinal)
    }

    fn record_packet_recovery(&self, cell_id: &CellId, ordinal: usize, recovery: JsonValue) {
        if let Some(metrics) = self.packet_admission.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).cells.get_mut(cell_id.as_str())
        {
            metrics.nested_recovery.insert(ordinal, recovery);
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep the per-call accounting and terminal evidence explicit"
    )]
    fn complete_packet_call(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        batchable_observation: bool,
        result_bytes: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
        nested_result: Option<CodeModeNestedResultEvidence>,
        required_terminal: Option<(RequiredToolTerminalCause, String)>,
    ) {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metrics) = admission.cells.get_mut(cell_id.as_str()) else {
            return;
        };
        metrics.delivery_blocked |= required_terminal.is_some()
            || nested_result.as_ref().is_some_and(|result| result.failed)
            || !post_tool_use_feedback.is_empty();
        metrics.batchable_observation_count = metrics
            .batchable_observation_count
            .saturating_add(usize::from(batchable_observation));
        metrics.result_bytes = metrics.result_bytes.saturating_add(result_bytes);
        metrics
            .post_tool_use_feedback
            .extend(post_tool_use_feedback);
        if let Some(nested_result) = nested_result {
            metrics.nested_recovery.entry(ordinal).or_insert_with(|| serde_json::json!({
                "call_id": nested_result.call_id, "tool_name": nested_result.tool_name,
                "failed": nested_result.failed, "output": nested_result.output,
                "output_truncated": nested_result.output_truncated,
                "recovery_available": !nested_result.output_truncated,
            }));
            if let Some(state) = &nested_result.command_state {
                let session = command_state_session(state);
                if let Some(session) = session {
                    metrics
                        .command_states
                        .retain(|previous| command_state_session(previous) != Some(session));
                }
                metrics.command_states.push(state.clone());
            }
            if metrics.nested_results.len() == MAX_RETAINED_NESTED_RESULTS {
                metrics.omitted_nested_result_count =
                    metrics.omitted_nested_result_count.saturating_add(1);
            }
            if metrics.nested_results.len() < MAX_RETAINED_NESTED_RESULTS {
                metrics.nested_results.push(nested_result);
            } else if let Some((latest_index, latest)) = metrics
                .nested_results
                .iter()
                .enumerate()
                .max_by_key(|(_, result)| (!result.failed, result.ordinal))
                && (!nested_result.failed, nested_result.ordinal) < (!latest.failed, latest.ordinal)
            {
                metrics.nested_results[latest_index] = nested_result;
            }
        }
        if let Some((cause, message)) = required_terminal {
            let candidate = CodeModeNestedTerminal {
                ordinal,
                cause,
                message,
            };
            if metrics
                .first_required_terminal
                .as_ref()
                .is_none_or(|current| candidate.ordinal < current.ordinal)
            {
                metrics.first_required_terminal = Some(candidate);
            }
        }
    }

    #[cfg(test)]
    fn record_packet_call(
        &self,
        cell_id: &CellId,
        batchable_observation: bool,
        result_bytes: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    ) {
        self.record_cell_parent_call_id(cell_id, "test-exec");
        let ordinal = self.begin_packet_call(cell_id).expect("registered cell");
        self.complete_packet_call(
            cell_id,
            ordinal,
            batchable_observation,
            result_bytes,
            post_tool_use_feedback,
            None,
            None,
        );
    }

    fn finish_packet(&self, cell_id: &str, retain_terminal: bool) -> CodeModePacketReceipt {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut metrics = admission
            .cells
            .get_mut(cell_id)
            .map(|metrics| {
                let next_nested_ordinal = metrics.next_nested_ordinal;
                let mut packet = std::mem::take(metrics);
                // Live output and steering can cross outstanding child calls.
                // Drain response data, but keep registration order for the cell.
                metrics.next_nested_ordinal = next_nested_ordinal;
                metrics.output_budget = packet.output_budget;
                metrics.turn_id = packet.turn_id.take();
                metrics.delivery_intent = packet.delivery_intent.take();
                metrics.delivery_blocked = packet.delivery_blocked;
                metrics.delivery_recovery = std::mem::take(&mut packet.delivery_recovery);
                if metrics.delivery_intent.is_some() {
                    metrics.command_states = packet.command_states.clone();
                }
                if retain_terminal {
                    metrics.first_required_terminal = packet.first_required_terminal.clone();
                }
                packet
            })
            .unwrap_or_default();
        metrics
            .nested_results
            .sort_unstable_by_key(|result| result.ordinal);
        CodeModePacketReceipt {
            command_states: metrics.command_states,
            nested_call_count: metrics.nested_call_count,
            batchable_observation_count: metrics.batchable_observation_count,
            result_bytes: metrics.result_bytes,
            post_tool_use_feedback: metrics.post_tool_use_feedback,
            nested_results: metrics.nested_results,
            nested_recovery: metrics.nested_recovery,
            omitted_nested_result_count: metrics.omitted_nested_result_count,
            first_required_terminal: metrics.first_required_terminal,
        }
    }

    pub(crate) fn record_owner_drained_continuation(
        &self,
        cell_id: &CellId,
        continuation: crate::session::turn_execution::PendingOwnerDrainedContinuation,
    ) {
        self.dispatch_broker
            .record_continuation(cell_id, continuation);
    }

    pub(crate) fn owner_drained_continuation_snapshot(
        &self,
        owner_key: &str,
    ) -> Vec<crate::session::turn_execution::PendingOwnerDrainedContinuation> {
        self.dispatch_broker
            .continuation_snapshot(&CellId::new(owner_key.to_string()))
    }

    pub(crate) fn acknowledge_owner_drained_continuations(
        &self,
        owner_key: &str,
        accepted: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        self.dispatch_broker
            .acknowledge_continuations(&CellId::new(owner_key.to_string()), accepted);
    }

    pub(crate) fn start_turn_worker(
        &self,
        session: &Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        request_signals: crate::session::turn_execution::SamplingRequestSignalCollector,
    ) -> Option<CodeModeDispatchWorker> {
        let turn = &step_context.turn;
        let tool_mode = effective_tool_mode(turn);
        if !matches!(tool_mode, ToolMode::CodeMode | ToolMode::CodeModeOnly) {
            return None;
        }

        let exec = ExecContext {
            session: Arc::clone(session),
            turn: Arc::clone(turn),
        };
        Some(
            self.dispatch_broker
                .start_turn_worker(exec, step_context, tracker, request_signals),
        )
    }

    async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        let session = self
            .session
            .get_or_try_init(|| async {
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err("code mode session is shutting down".to_string());
                }
                // Publish successful creation even if shutdown races it. The shutdown
                // waiter owns cleanup and must receive any error from that cleanup.
                self.session_provider
                    .create_session(self.dispatch_broker.clone())
                    .await
            })
            .await?;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        Ok(Arc::clone(session))
    }
}

pub(super) fn handle_runtime_response(
    exec: &ExecContext,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    started_at: std::time::Instant,
) -> Result<FunctionToolOutput, String> {
    // Nested tool results have already crossed their owning tool boundary. Keep
    // one coherent, model-safe exec packet here instead of applying the much
    // smaller generic per-tool diagnostic budget a second time.
    let hard_limit = exec
        .turn
        .config
        .tool_output_token_limit
        .unwrap_or(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    let original_image_detail_supported = can_request_original_image_detail(&exec.turn.model_info);

    let cell_id = runtime_response_cell_id(&response);
    let packet = exec.session.services.code_mode_service.finish_packet(
        cell_id,
        matches!(
            &response,
            RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. }
        ),
    );
    tracing::info!(
        target: "codex.code_mode.packet",
        cell_id,
        nested_call_count = packet.nested_call_count,
        batchable_observation_count = packet.batchable_observation_count,
        result_bytes = packet.result_bytes,
        post_tool_use_feedback_count = packet.post_tool_use_feedback.len(),
        retained_nested_result_count = packet.nested_results.len(),
        omitted_nested_result_count = packet.omitted_nested_result_count,
        "code-mode packet admission receipt"
    );
    let mut post_tool_use_feedback = packet.post_tool_use_feedback;
    let needs_retained_results = response_needs_retained_nested_results(&response);
    let nested_results = if needs_retained_results {
        if packet.omitted_nested_result_count > 0 {
            post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
                text: serde_json::json!({
                    "nested_result_recovery_directory": packet.nested_recovery.into_values().collect::<Vec<_>>(),
                    "omitted_inline_result_count": packet.omitted_nested_result_count,
                    "notice": "Inline diagnostics retain at most two results. Recover settled outcomes through this directory; do not rerun successful siblings. Artifact bytes are historical evidence, not fresh execution.",
                }).to_string(),
            });
        }
        packet.nested_results
    } else {
        Vec::new()
    };
    // Host lifecycle state stays in the canonical record. Only live handles
    // need a separate inline receipt when the script did not print them.
    let canonical_states = packet.command_states;
    let completed_cleanly = matches!(&response, RuntimeResponse::Result {
        error_text: None, output_loss: None, ..
    });
    for state in &canonical_states {
        // A script printing only result.output must not hide a crashed or
        // unstarted command behind later successful output.
        if (state["exit_code"].as_i64().is_some_and(|code| code != 0)
            && !(state["search_no_match"] == true && state["exit_code"] == 1))
            || state["execution_state"] == "unknown"
            || state.get("error").is_some_and(|error| !error.is_null())
        {
            post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
                text: serde_json::json!({"nested_command_failure": {
                    "call_id": state["call_id"],
                    "exit_code": state["exit_code"],
                    "execution_state": state["execution_state"],
                    "process_exited": state["process_exited"],
                    "error": state.get("error").and_then(JsonValue::as_str).map(|error| {
                        codex_utils_string::truncate_middle_chars(error, 1_024)
                    }),
                }}).to_string(),
            });
        }
    }
    // Reserve the controls before admitting ordinary text to the shared budget.
    // Controls and the failure itself remain visible even at a zero text budget.
    let control_tokens = codex_utils_output_truncation::model_token_count(
        &code_mode_text_content(&command_control_receipts(&canonical_states, "", completed_cleanly)),
    );
    let diagnostic_tokens = match &response {
        RuntimeResponse::Result { error_text: Some(error), .. } =>
            codex_utils_output_truncation::model_token_count(&format!("Script error:\n{error}")),
        _ => 0,
    };
    let requested = max_output_tokens.unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL).min(hard_limit);
    let text_budget = requested.saturating_sub(control_tokens)
        .max(diagnostic_tokens.min(requested))
        .max(usize::from(requested > 0)); // Preserve an omission marker beside mandatory controls.
    let completed_cell_id = cell_id.to_string();
    let mut output = format_runtime_response(
        response,
        Some(text_budget),
        hard_limit,
        original_image_detail_supported,
        started_at,
        post_tool_use_feedback,
        nested_results,
        packet.first_required_terminal,
    );
    if completed_cleanly && !needs_retained_results {
        // The script consumed its recovery results and emitted its own result.
        // Keep drain accounting without appending raw recovered pages to that
        // computed answer. Failed, silent, and yielded cells retain the fallback.
        let service = &exec.session.services.code_mode_service;
        let receipts = service.owner_drained_continuation_snapshot(&completed_cell_id)
            .into_iter().map(|continuation| continuation.receipt).collect::<Vec<_>>();
        service.acknowledge_owner_drained_continuations(&completed_cell_id, &receipts);
        output.deterministic_continuation_receipts.extend(receipts);
    }
    output.body.extend(command_control_receipts(
        &canonical_states, &code_mode_text_content(&output.body), completed_cleanly,
    ));
    output.essential_inline.insert(
        "nested_commands".into(),
        JsonValue::Array(canonical_states.clone()),
    );
    if !canonical_states.is_empty()
        && let Some(canonical) = &mut output.canonical_body
    {
        canonical.push(FunctionCallOutputContentItem::InputText {
            text: serde_json::json!({"nested_commands": canonical_states}).to_string(),
        });
    }
    Ok(output)
}

fn command_control_receipts(
    states: &[JsonValue],
    visible: &str,
    completed_cleanly: bool,
) -> Vec<FunctionCallOutputContentItem> {
    let mut items = Vec::new();
    // A zero text budget must not erase the only handle for still-owned work.
    for state in states {
        // A terminal process may still own an undrained pipe. Its handle is
        // actionable until the command owner removes it, even after exit.
        if command_owns_work(state)
            && !shows_session_handle(visible, state)
        {
            items.push(FunctionCallOutputContentItem::InputText {
                text: format!("Running command session_id: {}", state["session_id"]),
            });
            if state["session_capabilities"].is_object() {
                items.push(FunctionCallOutputContentItem::InputText {
                    text: serde_json::json!({
                        "session_id": state["session_id"],
                        "session_capabilities": state["session_capabilities"],
                        "continuation": state["continuation"],
                    }).to_string(),
                });
            }
        }
        // Printing only result.output must not discard the recovery route for
        // bytes omitted by the nested command, even if the outer packet fits.
        if state["output_reduced"] == true
            && !(completed_cleanly && state["display_suppressed"] == true)
        {
            let mut receipt = serde_json::json!({
                "nested_command_display_reduced": true,
                "call_id": state["call_id"],
                "cumulative_streams_complete": state["streams_complete"],
            });
            if let Some(artifact_id) = state["raw_output_artifact_id"].as_str() {
                if shows_command_recovery(visible, state) {
                    continue;
                }
                receipt["artifact_id"] = artifact_id.into();
                receipt["recovery_tool"] = "read_tool_output".into();
            } else {
                // Do not offer a nonexistent locator or silently imply that
                // omitted bytes can be recovered by another tool call.
                receipt["recovery_available"] = false.into();
            }
            if let Some(error) = state["raw_output_artifact_error"].as_str() {
                receipt["raw_output_artifact_error"] =
                    codex_utils_string::truncate_middle_chars(error, 256).into();
            }
            // The command owner already knows the omitted range. Preserve its
            // executable recovery route, not just a locator that forces another
            // discovery round trip or a reread of already delivered bytes.
            if let Some(recovery) = state.get("recovery") {
                receipt["recovery"] = recovery.clone();
            }
            if state["raw_output_artifact_retention_limit_hit"] == true {
                receipt["raw_output_artifact_retention_limit_hit"] = JsonValue::Bool(true);
            }
            items.push(FunctionCallOutputContentItem::InputText {
                text: receipt.to_string(),
            });
        }
    }
    items
}

fn failed_code_mode_cell_item(
    call_id: &str,
    response: &RuntimeResponse,
    duration: Duration,
) -> Option<DynamicToolCallItem> {
    let (cell_id, state, status, error) = match response {
        RuntimeResponse::Result {
            cell_id,
            error_text: Some(error),
            ..
        } => (cell_id.as_str(), "failed", DynamicToolCallStatus::Failed, Some(bounded_failed_cell_error(error))),
        RuntimeResponse::Terminated { cell_id, .. } => (
            cell_id.as_str(),
            "terminated",
            DynamicToolCallStatus::Failed,
            Some("code-mode cell terminated before completion".to_string()),
        ),
        RuntimeResponse::Yielded { cell_id, .. } =>
            (cell_id.as_str(), "in_progress", DynamicToolCallStatus::InProgress, None),
        RuntimeResponse::ExplicitYield { cell_id, .. } =>
            (cell_id.as_str(), "yielded", DynamicToolCallStatus::InProgress, None),
        RuntimeResponse::Result { cell_id, error_text: None, .. } =>
            (cell_id.as_str(), "completed", DynamicToolCallStatus::Completed, None),
    };

    Some(DynamicToolCallItem {
        id: format!("code-mode-cell:{cell_id}"),
        namespace: Some(FAILED_CELL_ITEM_NAMESPACE.to_string()),
        tool: FAILED_CELL_ITEM_TOOL.to_string(),
        arguments: serde_json::json!({
            "call_id": call_id,
            "cell_id": cell_id,
            "state": state,
        }),
        success: match status {
            DynamicToolCallStatus::InProgress => None,
            DynamicToolCallStatus::Completed => Some(true),
            DynamicToolCallStatus::Failed => Some(false),
        },
        status,
        content_items: None,
        error,
        duration: Some(duration),
    })
}

fn bounded_failed_cell_error(error: &str) -> String {
    if error.len() <= MAX_FAILED_CELL_ERROR_BYTES {
        return error.to_string();
    }

    let content_limit =
        MAX_FAILED_CELL_ERROR_BYTES.saturating_sub(FAILED_CELL_ERROR_TRUNCATION_MARKER.len());
    let mut end = content_limit.min(error.len());
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &error[..end], FAILED_CELL_ERROR_TRUNCATION_MARKER)
}

pub(super) async fn emit_failed_code_mode_cell_item(
    exec: &ExecContext,
    call_id: &str,
    response: &RuntimeResponse,
    started_at: std::time::Instant,
) {
    let Some(item) = failed_code_mode_cell_item(call_id, response, started_at.elapsed()) else {
        return;
    };
    if item.status == DynamicToolCallStatus::InProgress {
        exec.session
            .emit_turn_item_started(exec.turn.as_ref(), &TurnItem::DynamicToolCall(item))
            .await;
    } else {
        exec.session
            .emit_turn_item_completed(exec.turn.as_ref(), TurnItem::DynamicToolCall(item))
            .await;
    }
}

fn merge_code_mode_signal(mut output: FunctionToolOutput, signal: JsonValue) -> FunctionToolOutput {
    if let (Some(existing), Some(additional)) = (
        output
            .sampling_request_signal
            .as_mut()
            .and_then(JsonValue::as_object_mut),
        signal.as_object(),
    ) {
        existing.extend(additional.clone());
    } else {
        output.sampling_request_signal = Some(signal);
    }
    output
}

fn fold_nested_required_terminal(
    output: FunctionToolOutput,
    terminal: CodeModeNestedTerminal,
) -> FunctionToolOutput {
    let (output, outcome) = match terminal.cause {
        RequiredToolTerminalCause::Blocked => (
            output.with_skip_disposition(ToolOutputSkipDisposition::BlockingRequiredOperation),
            "blocked",
        ),
        RequiredToolTerminalCause::Failure => {
            (output.with_outcome(ToolOutputOutcome::Failure), "failure")
        }
    };
    merge_code_mode_signal(
        output,
        serde_json::json!({
            "outcome": outcome,
            "nested_ordinal": terminal.ordinal,
        }),
    )
}

fn required_nested_tool_terminal_cause(
    outcome_context: ToolOutputOutcomeContext,
    signal: Option<&JsonValue>,
) -> Option<RequiredToolTerminalCause> {
    required_tool_terminal_cause(outcome_context, signal)
}

fn runtime_response_cell_id(response: &RuntimeResponse) -> &str {
    match response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::ExplicitYield { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.as_str(),
    }
}

fn response_needs_retained_nested_results(response: &RuntimeResponse) -> bool {
    match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::ExplicitYield { content_items, .. } => content_items.is_empty(),
        RuntimeResponse::Terminated { .. } => true,
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => error_text.is_some() || content_items.is_empty(),
    }
}

fn nested_result_content_items(
    nested_results: Vec<CodeModeNestedResultEvidence>,
) -> Vec<FunctionCallOutputContentItem> {
    nested_results
        .into_iter()
        .map(|result| {
            let value = serde_json::from_str(&result.output)
                .unwrap_or_else(|_| JsonValue::String(result.output));
            let value = model_visible_nested_result(&ToolName::plain(&result.tool_name), value);
            FunctionCallOutputContentItem::InputText {
                text: serde_json::json!({
                    "call_id": result.call_id,
                    "tool_name": result.tool_name,
                    "output_truncated": result.output_truncated,
                    "result": value,
                }).to_string(),
            }
        })
        .collect()
}

#[expect(
    clippy::too_many_arguments,
    reason = "Rendering consumes output budgets, timing, feedback, and terminal evidence together"
)]
fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    hard_limit: usize,
    original_image_detail_supported: bool,
    started_at: std::time::Instant,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    required_terminal: Option<CodeModeNestedTerminal>,
) -> FunctionToolOutput {
    let continuation_owner_key = match &response {
        RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::ExplicitYield { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. }
        | RuntimeResponse::Result { cell_id, .. } => cell_id.to_string(),
    };
    let script_status = format_script_status(&response);
    let output_loss = match &response {
        RuntimeResponse::Result { output_loss, .. } => output_loss.clone(),
        _ => None,
    };
    let yielded = matches!(
        &response,
        RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. }
    );
    // A yielded cell still owns its work. Publish required failures with its
    // terminal response, even if a nested call finishes just after yielding.
    let required_terminal = if yielded { None } else { required_terminal };
    let (mut content_items, mut outcome, mut success, script_error) = match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::ExplicitYield { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Success, true, None)
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let content_items = into_function_call_output_content_items(content_items);
            (content_items, OutputOutcome::Failure, false, None)
        }
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let content_items = into_function_call_output_content_items(content_items);
            let success = error_text.is_none();
            let outcome = if success {
                OutputOutcome::Success
            } else {
                OutputOutcome::Failure
            };
            (content_items, outcome, success, error_text)
        }
    };

    // Read only the runtime's error envelope, never script-printed output.
    // Reuse the existing nested-result ledger rather than maintaining another
    // effect journal. A returned result proves observation, not rollback.
    let store_commit_failure = script_error.as_deref()
        .and_then(|error| serde_json::from_str::<JsonValue>(error).ok())
        .filter(|receipt| {
            receipt.get("kind").and_then(JsonValue::as_str)
                == Some("code_mode_store_commit_failure")
                && receipt.get("version").and_then(JsonValue::as_u64) == Some(1)
        })
        .map(|mut receipt| {
            receipt["automatic_replay_allowed"] = JsonValue::Bool(false);
            receipt["observed_nested_call_ids"] = serde_json::json!(
                nested_results.iter().map(|result| &result.call_id).collect::<Vec<_>>()
            );
            // Retention is bounded: absent IDs are unknown, not unexecuted.
            receipt["nested_effect_inventory_complete"] = JsonValue::Bool(false);
            receipt
        });
    let canonical_nested = nested_results
        .iter()
        .map(|result| FunctionCallOutputContentItem::InputText {
            text: serde_json::to_string(result).unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    // An uncaught nested rejection is rethrown as the script error, which
    // already carries its complete message; the retained copy only repeats it.
    // So does a copy of a result the script printed before it failed.
    let emitted = if nested_results.is_empty() {
        String::new()
    } else {
        code_mode_text_content(&content_items)
    };
    let nested_results = nested_results
        .into_iter()
        .filter(|result| {
            !(result.failed
                && script_error
                    .as_deref()
                    .is_some_and(|error| error.contains(result.output.as_str())))
        })
        .filter(|result| !nested_result_already_emitted(result, &emitted))
        .collect();
    content_items.extend(nested_result_content_items(nested_results));
    content_items.extend(post_tool_use_feedback);
    let mut diagnostic = required_terminal.as_ref().map(|terminal| {
        success = false;
        outcome = match terminal.cause {
            RequiredToolTerminalCause::Blocked => OutputOutcome::Skipped,
            RequiredToolTerminalCause::Failure => OutputOutcome::Failure,
        };
        format!("Required nested tool outcome: {}", terminal.message)
    });
    if let Some(error_text) = script_error {
        let script_error = format!("Script error:\n{error_text}");
        match &mut diagnostic {
            Some(diagnostic) => {
                diagnostic.push('\n');
                diagnostic.push_str(&script_error);
            }
            None => diagnostic = Some(script_error),
        }
    }
    let diagnostic_index = diagnostic.map(|text| {
        let index = content_items.len();
        content_items.push(FunctionCallOutputContentItem::InputText { text });
        index
    });
    sanitize_image_detail_items(original_image_detail_supported, &mut content_items);
    let mut canonical_content_items = content_items.clone();
    canonical_content_items.extend(canonical_nested);
    // The saved artifact includes a four-line status prefix (three header lines
    // plus the item separator), and optionally one output-loss metadata line.
    let line_offset = script_status.lines().count() + 3 + usize::from(output_loss.is_some());
    let total_lines = line_offset + code_mode_text_content(&canonical_content_items).lines().count();
    let (mut content_items, visible_output_truncated, omitted_lines) = truncate_code_mode_result_at_lines(
        content_items,
        max_output_tokens,
        outcome,
        hard_limit,
        diagnostic_index,
        Some((line_offset, total_lines)),
    );
    let semantic_evidence = serde_json::json!({
        "status": &script_status,
        "content_items": &content_items,
        "output_loss": &output_loss,
    });
    if let Some(output_loss) = output_loss {
        let metadata = FunctionCallOutputContentItem::InputText {
            text: serde_json::json!({
                "output_complete": false,
                "output_loss": output_loss,
                "discarded_output_recoverable": false,
            })
            .to_string(),
        };
        content_items.insert(0, metadata.clone());
        canonical_content_items.insert(0, metadata);
    }
    let elapsed = started_at.elapsed();
    if yielded || !success {
        content_items.insert(
            0,
            FunctionCallOutputContentItem::InputText {
                text: script_status.clone(),
            },
        );
    }
    prepend_script_status(&mut canonical_content_items, &script_status, elapsed);
    let typed_outcome = match (yielded, outcome) {
        (true, _) => codex_tools::ToolOutputOutcome::Yielded,
        (false, OutputOutcome::Success) => codex_tools::ToolOutputOutcome::Success,
        (false, OutputOutcome::Failure) => codex_tools::ToolOutputOutcome::Failure,
        (false, OutputOutcome::TimedOut) => codex_tools::ToolOutputOutcome::TimedOut,
        (false, OutputOutcome::Skipped) => codex_tools::ToolOutputOutcome::Skipped,
    };
    let sampling_request_signal = if success {
        crate::tools::context::semantic_evidence_sampling_signal(semantic_evidence)
    } else {
        crate::tools::context::semantic_failure_sampling_signal(semantic_evidence)
    };
    let mut output = FunctionToolOutput::from_content(content_items, Some(success))
        .with_canonical_body(canonical_content_items)
        .with_outcome(typed_outcome)
        .with_sampling_request_signal(sampling_request_signal)
        .with_deterministic_continuation_owner_key(continuation_owner_key);
    if let Some(receipt) = store_commit_failure {
        output.essential_inline.insert("store_commit".to_string(), receipt);
    }
    if visible_output_truncated {
        output.essential_inline.insert(
            VISIBLE_OUTPUT_TRUNCATED_KEY.to_string(),
            JsonValue::Bool(true),
        );
        if let Some((start, end)) = omitted_lines {
            output.essential_inline.insert("cell_output_recovery_selector".into(),
                serde_json::json!({"kind":"lines", "start":start, "end":end.min(start.saturating_add(199))}));
        }
    }
    match required_terminal {
        Some(terminal) => fold_nested_required_terminal(output, terminal),
        None => output,
    }
}

fn format_script_status(response: &RuntimeResponse) -> String {
    match response {
        RuntimeResponse::Yielded { cell_id, .. } => {
            format!("Script running with cell ID {cell_id}")
        }
        RuntimeResponse::ExplicitYield { cell_id, .. } => {
            format!("Script running with cell ID {cell_id} after explicit yield")
        }
        RuntimeResponse::Terminated { cell_id, .. } => {
            format!("Script terminated with cell ID {cell_id}")
        }
        RuntimeResponse::Result {
            cell_id,
            error_text,
            ..
        } => {
            if error_text.is_none() {
                format!("Script completed with cell ID {cell_id}")
            } else {
                format!("Script failed with cell ID {cell_id}")
            }
        }
    }
}

fn prepend_script_status(
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    status: &str,
    wall_time: Duration,
) {
    let wall_time_seconds = ((wall_time.as_secs_f32()) * 10.0).round() / 10.0;
    let header = format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n");
    content_items.insert(0, FunctionCallOutputContentItem::InputText { text: header });
}

/// Returns the model-visible packet and whether it omits any output.
fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    outcome: OutputOutcome,
    hard_limit: usize,
    diagnostic_index: Option<usize>,
) -> (Vec<FunctionCallOutputContentItem>, bool) {
    let (items, omitted, _) = truncate_code_mode_result_at_lines(items, max_output_tokens, outcome, hard_limit, diagnostic_index, None);
    (items, omitted)
}

fn truncate_code_mode_result_at_lines(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    outcome: OutputOutcome,
    hard_limit: usize,
    diagnostic_index: Option<usize>,
    source_lines: Option<(usize, usize)>,
) -> (Vec<FunctionCallOutputContentItem>, bool, Option<(usize, usize)>) {
    let diagnostic_text = code_mode_text_content(&items);
    let requested_limit =
        max_output_tokens.unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    let hard_limit = hard_limit.min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    let limits = resolve_output_limits(
        Some(requested_limit),
        outcome,
        None,
        &diagnostic_text,
        hard_limit,
    );
    // Avoid a recovery round trip when retaining the whole packet costs at
    // most one third more than its soft budget. Larger packets still use the
    // original budget, and neither a zero budget nor a hard cap is relaxed.
    let whole_packet_limit = limits.applied_limit
        .saturating_add(limits.applied_limit / 3)
        .min(hard_limit);
    if limits.applied_limit != 0
        && codex_utils_output_truncation::model_token_count(&diagnostic_text) <= whole_packet_limit
    {
        return (items, false, None);
    }
    let policy = TruncationPolicy::Tokens(limits.applied_limit);
    if let Some(error_index) = diagnostic_index {
        return truncate_code_mode_failure(items, error_index, limits.applied_limit, source_lines);
    }
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (offset, total) = source_lines.unwrap_or((0, diagnostic_text.lines().count()));
        let (truncated, omitted_lines) = codex_utils_output_truncation::truncate_model_text_at_lines_with_recovery(
            &diagnostic_text,
            limits.applied_limit,
            offset, total,
            Some("{cell_output_artifact_id}"),
        );
        let omitted = truncated != diagnostic_text;
        return (
            vec![FunctionCallOutputContentItem::InputText { text: truncated }],
            omitted,
            omitted_lines,
        );
    }

    let projected = truncate_function_output_items_with_policy(&items, policy);
    let omitted = projected != items;
    (projected, omitted, None)
}

fn truncate_code_mode_failure(
    mut items: Vec<FunctionCallOutputContentItem>,
    error_index: usize,
    token_limit: usize,
    source_lines: Option<(usize, usize)>,
) -> (Vec<FunctionCallOutputContentItem>, bool, Option<(usize, usize)>) {
    let FunctionCallOutputContentItem::InputText { text: error_text } = items.remove(error_index)
    else {
        unreachable!("the caller identifies a script-error text item")
    };
    let error_tokens = codex_utils_output_truncation::model_token_count(&error_text);
    let preceding = code_mode_text_content(&items);
    let (offset, total) = source_lines.unwrap_or((0, preceding.lines().count() + error_text.lines().count()));
    let error_offset = offset + preceding.bytes().filter(|byte| *byte == b'\n').count() + usize::from(!items.is_empty());
    let reserved_error_tokens = error_tokens.min(token_limit);
    let other_policy = TruncationPolicy::Tokens(token_limit.saturating_sub(reserved_error_tokens));
    let (mut projected, mut omitted, mut omitted_lines) = if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let text = code_mode_text_content(&items);
        let (truncated, omitted_lines) =
            codex_utils_output_truncation::truncate_model_text_at_lines_with_recovery(&text, other_policy.token_budget(), offset, total, Some("{cell_output_artifact_id}"));
        let omitted = truncated != text;
        (
            vec![FunctionCallOutputContentItem::InputText { text: truncated }],
            omitted,
            omitted_lines,
        )
    } else {
        let projected = truncate_function_output_items_with_policy(&items, other_policy);
        let omitted = projected != items;
        (projected, omitted, None)
    };
    let error_text = if error_tokens <= reserved_error_tokens {
        error_text
    } else {
        omitted = true;
        let (text, gap) = codex_utils_output_truncation::truncate_model_text_at_lines_with_recovery(
            &error_text, reserved_error_tokens, error_offset, total, Some("{cell_output_artifact_id}"));
        omitted_lines = omitted_lines.or(gap);
        text
    };
    if !error_text.is_empty() {
        projected.push(FunctionCallOutputContentItem::InputText { text: error_text });
    }
    (projected, omitted, omitted_lines)
}

/// Whether visible text already carries this live handle: in a printed result
/// envelope or an earlier structured receipt. Quoted source/history is not a
/// receipt, and an ID without its current capabilities is insufficient.
fn shows_session_handle(visible: &str, state: &JsonValue) -> bool {
    visible.lines().filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .any(|value| {
            let mut pending = vec![&value];
            while let Some(value) = pending.pop() {
                if value.get("session_id") == state.get("session_id")
                    && value["session_capabilities"].is_object()
                    && value.get("session_capabilities") == state.get("session_capabilities")
                    && (value["execution_state"] == "running"
                        || value.get("continuation").is_some_and(|continuation|
                            !continuation.is_null() && Some(continuation) == state.get("continuation")))
                {
                    return true;
                }
                match value {
                    JsonValue::Object(fields) => pending.extend(fields.values()),
                    JsonValue::Array(items) => pending.extend(items),
                    _ => {}
                }
            }
            false
        })
}

fn shows_command_recovery(visible: &str, state: &JsonValue) -> bool {
    // A locator mentioned in source text is not an executable recovery route.
    // Batched whole results may nest the envelope. Traverse JSON containers,
    // never strings (which can contain quoted source or historical output).
    visible.lines().filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .any(|value| {
            let mut pending = vec![&value];
            while let Some(value) = pending.pop() {
                let found = if let Some(recovery) = state.get("recovery") {
                    value.get("recovery") == Some(recovery)
                        || (value.get("raw_output_artifact_id") == state.get("raw_output_artifact_id")
                            && value.get("recovery_selector").is_some()
                            && value.get("recovery_selector")
                                == recovery.pointer("/arguments/selectors/0"))
                } else {
                    state["raw_output_artifact_id"].is_string()
                        && (value.get("artifact_id") == state.get("raw_output_artifact_id")
                            || value.get("raw_output_artifact_id") == state.get("raw_output_artifact_id"))
                };
                if found {
                    return true;
                }
                match value {
                    JsonValue::Object(fields) => pending.extend(fields.values()),
                    JsonValue::Array(items) => pending.extend(items),
                    _ => {}
                }
            }
            false
        })
}

fn code_mode_text_content(items: &[FunctionCallOutputContentItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call_nested_tool(
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    invocation: CodeModeNestedToolCall,
    cancellation: NestedCancellation,
) -> Result<JsonValue, FunctionCallError> {
    let cancellation_token = cancellation.token().clone();
    let CodeModeNestedToolCall {
        cell_id,
        parent_tool_call_id,
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
        nested_deadline,
        buffered_output_bytes: _,
    } = invocation;
    let packet_ordinal = exec
        .session
        .services
        .code_mode_service
        .begin_packet_call(&cell_id)
        .ok_or_else(|| FunctionCallError::RespondToModel("code mode cell is closed".to_string()))?;
    if is_exec_tool_name(&tool_name) {
        let message = format!("{PUBLIC_TOOL_NAME} cannot invoke itself");
        exec.session
            .services
            .code_mode_service
            .complete_packet_call(
                &cell_id,
                packet_ordinal,
                false,
                0,
                Vec::new(),
                None,
                Some((RequiredToolTerminalCause::Failure, message.clone())),
            );
        return Err(FunctionCallError::RespondToModel(message));
    }

    let nested_call_id = format!(
        "{PUBLIC_TOOL_NAME}-{}-{runtime_tool_call_id}",
        cell_id.as_str()
    );
    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => {
            tool_runtime.record_code_mode_failure(
                cell_id.as_str(),
                &tool_name,
                None,
                nested_failure_fingerprint(&tool_name, &error),
            ).await;
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
                    false,
                    0,
                    Vec::new(),
                    Some(CodeModeNestedResultEvidence {
                        failed: true,
                        command_state: None,
                        output_fingerprints: Vec::new(),
                        ordinal: packet_ordinal,
                        call_id: nested_call_id,
                        parent_call_id: parent_tool_call_id,
                        parent_cell_id: cell_id.to_string(),
                        runtime_tool_call_id,
                        tool_name: tool_name.to_string(),
                        output: error
                            .chars()
                            .take(MAX_RETAINED_NESTED_RESULT_BYTES / 4)
                            .collect(),
                        output_truncated: error.len() > MAX_RETAINED_NESTED_RESULT_BYTES / 4,
                    }),
                    None,
                );
            return Err(FunctionCallError::RespondToModel(error));
        }
    };
    if let Some(message) = wrapped_patch_rejection(
        &tool_name,
        &payload,
        tool_runtime.has_registered_tool(&ToolName::plain("apply_patch")),
    ) {
        tool_runtime.record_code_mode_failure(
            cell_id.as_str(),
            &tool_name,
            Some(&payload),
            nested_failure_fingerprint(&tool_name, &message),
        ).await;
        exec.session
            .services
            .code_mode_service
            .complete_packet_call(
                &cell_id,
                packet_ordinal,
                false,
                0,
                Vec::new(),
                None,
                Some((RequiredToolTerminalCause::Failure, message.clone())),
            );
        return Err(FunctionCallError::RespondToModel(message));
    }

    let call = ToolCall {
        tool_name: tool_name.clone(),
        call_id: nested_call_id.clone(),
        payload: payload.clone(),
    };
    // Filled by dispatch only after workspace admission, including any edits
    // that finish while this nested call waits for its lease.
    let admitted_revision = Arc::new(std::sync::Mutex::new(None));
    let result = tool_runtime
        .clone()
        .handle_tool_call_with_admitted_revision(
            call,
            ToolCallSource::CodeMode {
                cell_id: cell_id.to_string(),
                parent_call_id: parent_tool_call_id.clone(),
                runtime_tool_call_id: runtime_tool_call_id.clone(),
                nested_deadline,
                // Whoever stops this call records why before signalling; the
                // aborted-result formatter reads it rather than assuming the
                // user interrupted.
                cancellation_cause: Some(cancellation.cause_cell()),
            },
            cancellation_token,
            Some(Arc::clone(&admitted_revision)),
        )
        .await;
    let mut result = match result {
        Ok(result) => result,
        Err(error) => {
            let message = error.to_string();
            let terminal_cause = required_tool_error_terminal_cause(&error);
            tool_runtime.record_code_mode_failure(
                cell_id.as_str(),
                &tool_name,
                Some(&payload),
                nested_failure_fingerprint(&tool_name, &message),
            ).await;
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
                    false,
                    0,
                    Vec::new(),
                    Some(CodeModeNestedResultEvidence {
                        failed: true,
                        command_state: None,
                        output_fingerprints: Vec::new(),
                        ordinal: packet_ordinal,
                        call_id: nested_call_id,
                        parent_call_id: parent_tool_call_id,
                        parent_cell_id: cell_id.to_string(),
                        runtime_tool_call_id,
                        tool_name: tool_name.to_string(),
                        output: message
                            .chars()
                            .take(MAX_RETAINED_NESTED_RESULT_BYTES / 4)
                            .collect(),
                        output_truncated: message.len() > MAX_RETAINED_NESTED_RESULT_BYTES / 4,
                    }),
                    terminal_cause.map(|cause| (cause, message)),
                );
            return Err(error);
        }
    };
    let outcome_context = result.outcome_context();
    let mut signal = result.sampling_request_signal();
    if let Some(revision) = *admitted_revision.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
        signal.get_or_insert_with(|| serde_json::json!({}))["validation_mutation_revision"] =
            serde_json::json!(revision);
    }
    let canonical_artifact_required = result.requires_canonical_artifact();
    let retained_artifact = result.canonical_delivery_artifact();
    let delivery_artifact = canonical_artifact_required.then(|| retained_artifact.clone()).flatten();
    let receipts = result.intrinsic_deterministic_continuation_receipts();
    let source_dependencies = result.source_dependencies.clone();
    if let Some(continuation) = result.owner_drained_continuation() {
        exec.turn
            .turn_timing_state
            .record_owner_drained_continuation();
        exec.session
            .services
            .code_mode_service
            .record_owner_drained_continuation(&cell_id, continuation);
    }
    let post_tool_use_feedback = result.take_code_mode_feedback();
    let failure_is_error = result.code_mode_failure_is_error();
    // Earlier prints must not erase a later command's JavaScript result. The
    // outer projection bounds and retains the combined printed output; shrinking
    // the tool result here can discard small, unspilled output before it reaches
    // that boundary. Keep the cell-sized per-result cap and envelope reserve.
    let budget = exec.session.services.code_mode_service.output_budget(cell_id.as_str())
        .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL) * 4 / 5;
    let mut result_value = result.code_mode_result_with_budget(budget);
    let recovery = exec.session.services.code_mode_service.record_delivery_evidence(
        &cell_id, canonical_artifact_required, delivery_artifact, signal.as_ref(), &result_value,
    );
    if let Some(recovery) = recovery {
        // Keep the native representation while exposing the retained recovery
        // route to JavaScript, not only to the later model-facing projection.
        if let Some(fields) = result_value.as_object_mut() {
            fields.insert("canonical_recovery".to_string(), recovery);
        } else if let Some(text) = result_value.as_str() {
            if let Ok(JsonValue::Object(mut fields)) = serde_json::from_str::<JsonValue>(text) {
                fields.insert("canonical_recovery".to_string(), recovery);
                result_value = JsonValue::String(JsonValue::Object(fields).to_string());
            } else {
                result_value = JsonValue::String(format!("{text}\n{}", serde_json::json!({"canonical_recovery": recovery})));
            }
        }
    }
    if (tool_name == ToolName::plain("exec_command")
        || tool_name == ToolName::plain("write_stdin"))
        && result_value.get("output_reduced") == Some(&JsonValue::Bool(true))
    {
        exec.turn
            .turn_timing_state
            .record_nested_tool_output_reduction();
    }
    let (retained_output, output_truncated, result_bytes) = bounded_serialized_json(&result_value);
    // Reuse dispatcher retention. Only results without a canonical receipt need
    // new storage, and only when their complete inline outcome does not fit.
    let mut recovery_artifact = retained_artifact;
    if recovery_artifact.is_none() && output_truncated {
        let canonical = codex_tools::CanonicalToolResult::json(result_value.clone());
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &exec.turn.config.codex_home, &exec.session.thread_id.to_string(), &canonical,
        ).await;
        if artifact.complete && let Some(id) = artifact.artifact_id() {
            exec.session.register_tool_artifact_origin(id.clone(), nested_call_id.clone(),
                canonical.exact_bytes, canonical.sha256.clone()).await;
            recovery_artifact = Some((id, canonical.sha256, canonical.exact_bytes));
        }
    }
    let recovery = if let Some((id, sha256, bytes)) = &recovery_artifact {
        serde_json::json!({
            "artifact_id": id, "canonical_sha256": sha256, "canonical_bytes": bytes,
            "recovery": {"tool":"read_tool_output", "arguments": {
                "artifact_id":id, "selectors":[{"kind":"bytes","start":0,"end":bytes}]
            }},
        })
    } else if !output_truncated {
        serde_json::json!({"result": result_value})
    } else {
        serde_json::json!({"output": retained_output, "output_truncated":true,
            "recovery_available":false, "notice":"Canonical retention failed; do not replay effects."})
    };
    exec.session.services.code_mode_service.record_packet_recovery(&cell_id, packet_ordinal,
        serde_json::json!({"call_id":nested_call_id,"tool_name":tool_name.to_string(),
            "failed":!matches!(outcome_context.outcome, ToolOutputOutcome::Success | ToolOutputOutcome::Yielded),
            "outcome":recovery}));
    if let Some(parent_call_id) = parent_tool_call_id.as_ref()
        && source_dependencies
            .as_ref()
            .is_some_and(|dependencies| !dependencies.is_empty())
    {
        let evidence_output = if !output_truncated && retained_output.len() <= 4_096 {
            Some(retained_output.clone())
        } else if tool_name == ToolName::plain("read_file")
            && result_value["source_sha256"].is_string()
        {
            Some(crate::tool_history::compact_read_evidence(&result_value).to_string())
        } else {
            recovery_artifact.as_ref().map(|(id, _, bytes)| serde_json::json!({
                "artifact_id":id, "historical_source":true, "recovery_tool":"read_tool_output",
                "selectors":[{"kind":"bytes","start":0,"end":bytes}],
            }).to_string())
        };
        if let Some(output) = evidence_output {
            exec.session
                .register_code_mode_nested_evidence(
                    parent_call_id.clone(),
                    nested_call_id.clone(),
                    output,
                )
                .await;
        }
    }
    let script_result = script_visible_nested_result(
        &tool_name, result_value.clone(), &nested_call_id, packet_ordinal,
    );
    let nested_result = CodeModeNestedResultEvidence {
        failed: !matches!(
            outcome_context.outcome,
            codex_tools::ToolOutputOutcome::Success | codex_tools::ToolOutputOutcome::Yielded
        ),
        command_state: nested_command_state(tool_runtime.command_semantic_name(&tool_name).as_ref(), &nested_call_id, &payload, &result_value),
        output_fingerprints: nested_output_fingerprints(&tool_name, &result_value),
        ordinal: packet_ordinal,
        call_id: nested_call_id.clone(),
        parent_call_id: parent_tool_call_id,
        parent_cell_id: cell_id.to_string(),
        runtime_tool_call_id,
        tool_name: tool_name.to_string(),
        output: retained_nested_output(&tool_name, &result_value, retained_output),
        output_truncated,
    };
    let required_terminal = required_nested_tool_terminal_cause(outcome_context, signal.as_ref())
        .filter(|cause| failure_is_error || matches!(cause, RequiredToolTerminalCause::Blocked))
        .map(|cause| {
            let label = match cause {
                RequiredToolTerminalCause::Blocked => "blocked",
                RequiredToolTerminalCause::Failure => "failed",
            };
            (
                cause,
                format!("required nested tool `{}` {label}", tool_name.name),
            )
        });
    exec.session
        .services
        .code_mode_service
        .complete_packet_call(
            &cell_id,
            packet_ordinal,
            is_batchable_observation(&tool_name, &payload)
                && !result_has_live_exec_session(&result_value),
            result_bytes,
            post_tool_use_feedback,
            Some(nested_result),
            required_terminal,
        );
    tool_runtime.record_code_mode_result(
        &nested_call_id,
        CodeModeToolResult {
            cell_id: cell_id.as_str(),
            tool_name: &tool_name,
            payload: &payload,
            source_dependencies,
            outcome_context,
            signal: signal.as_ref(),
            result: &result_value,
            canonical_artifact_required,
        },
        &receipts,
    );
    if failure_is_error
        && matches!(
            outcome_context.outcome,
            ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut
        )
    {
        return Err(FunctionCallError::RespondToModel(script_result.to_string()));
    }
    // This value is consumed by JavaScript, not rendered directly to the model.
    // Preserve the owning tool's structured contract so a cell can inspect
    // process completion, recover omitted output, and use patch change metadata
    // without returning to the model for another decision.
    Ok(script_result)
}

/// Remove nondeterministic diagnostics, not lifecycle or recovery controls.
/// The unprojected result has already been retained for session diagnostics.
fn script_visible_nested_result(
    tool: &ToolName,
    mut value: JsonValue,
    call_id: &str,
    position: usize,
) -> JsonValue {
    if tool.namespace.is_none()
        && matches!(tool.name.as_str(), "exec_command" | "write_stdin")
        && let Some(object) = value.as_object_mut()
    {
        for key in ["wall_time_seconds", "original_token_count", "original_token_count_is_approximate"] {
            object.remove(key);
        }
        if object.contains_key("chunk_id") {
            let mut hash = Sha256::new();
            hash.update(b"command-chunk-v1\0");
            hash.update((call_id.len() as u64).to_be_bytes());
            hash.update(call_id.as_bytes());
            hash.update((position as u64).to_be_bytes());
            object.insert("chunk_id".to_string(), format!("{:x}", hash.finalize()).into());
        }
    }
    value
}

/// Retained evidence of a completed nested call: a command's exit code and
/// output, otherwise the bounded result JSON.
fn retained_nested_output(
    tool_name: &ToolName,
    result_value: &JsonValue,
    retained_json: String,
) -> String {
    if tool_name.namespace.is_none()
        && matches!(tool_name.name.as_str(), "exec_command" | "write_stdin")
        && let Some(text) = result_value["output"].as_str()
    {
        let mut rendered = format!("exit_code: {}\n{}", result_value["exit_code"], text);
        if let Some(repair) = result_value["repair"].as_str() {
            rendered.push('\n');
            rendered.push_str(repair);
        }
        codex_utils_string::truncate_middle_chars(&rendered, MAX_RETAINED_NESTED_RESULT_BYTES - 128)
    } else {
        retained_json
    }
}

/// Exact identities let bounded fallback diagnostics recognize already printed commands.
fn nested_output_fingerprints(tool: &ToolName, value: &JsonValue) -> Vec<(String, usize, [u8; 32])> {
    if tool.namespace.is_some() || !matches!(tool.name.as_str(), "exec_command" | "write_stdin")
        || value["repair"].as_str().is_some_and(|repair| !repair.is_empty())
    {
        return Vec::new();
    }
    let Some(output) = value["output"].as_str().filter(|output| output.len() >= 32) else {
        return Vec::new();
    };
    let escaped = serde_json::to_string(output).expect("string serialization");
    [output, &escaped[1..escaped.len() - 1]].into_iter().map(|text| {
        (text.chars().take(32).collect(), text.len(), Sha256::digest(text.as_bytes()).into())
    }).collect()
}

/// Whether the script already printed this retained result before the cell
/// ended. `text(result)` shows it JSON-escaped and `text(result.output)`
/// verbatim. Require the entire bounded payload, not matching ends: siblings
/// can share wrappers while carrying different diagnostics in the middle.
fn nested_result_already_emitted(result: &CodeModeNestedResultEvidence, emitted: &str) -> bool {
    if result.output_fingerprints.iter().any(|(prefix, bytes, digest)| {
        // Repetition must not turn fallback deduplication into quadratic work.
        // Uncertain matches keep their diagnostic rather than dropping evidence.
        emitted.match_indices(prefix).take(32).any(|(start, _)| {
            emitted.as_bytes().get(start..start.saturating_add(*bytes))
                .is_some_and(|candidate| <[u8; 32]>::from(Sha256::digest(candidate)) == *digest)
        })
    }) {
        return true;
    }
    // A short result costs little to repeat and is weak evidence of printing.
    const MIN_PAYLOAD_BYTES: usize = 32;
    let matches_output = |output: &str| {
        let payload = match output
            .strip_prefix("exit_code: ")
            .and_then(|rest| rest.split_once('\n'))
        {
            Some((_, output)) => output,
            None => output,
        };
        if payload.len() < MIN_PAYLOAD_BYTES {
            return false;
        }
        emitted.contains(payload)
            || serde_json::to_string(payload)
                .is_ok_and(|escaped| emitted.contains(&escaped[1..escaped.len() - 1]))
    };
    matches_output(&result.output)
        || serde_json::from_str(&result.output)
            .ok()
            .and_then(|raw| codex_code_mode::model_visible_tool_result(
                &ToolName::plain(&result.tool_name), &raw,
            ))
            .is_some_and(|projected| matches_output(&projected.to_string()))
}

// Presentation only: never use this projection for a JavaScript tool return.
fn model_visible_nested_result(tool: &ToolName, value: JsonValue) -> JsonValue {
    if let Some(projected) = codex_code_mode::model_visible_tool_result(tool, &value) {
        return projected;
    }
    if tool.namespace.is_some() || !value.is_object() {
        return value;
    }
    if matches!(tool.name.as_str(), "exec_command" | "write_stdin") {
        // The shared projection returns None when no transport fields need
        // removal. That must not trigger a second, lossy lifecycle projection.
        return value;
    }
    value
}

fn nested_command_argv(tool_name: &ToolName, payload: &ToolPayload) -> Option<Vec<String>> {
    if tool_name.namespace.is_some()
        || !matches!(
            tool_name.name.as_str(),
            "exec_command" | "shell_command" | "unified_exec"
        )
    {
        return None;
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let arguments = serde_json::from_str::<JsonValue>(arguments).ok()?;
    arguments
        .get("program")
        .and_then(JsonValue::as_str)
        .map(|program| {
            let mut command = vec![program.to_string()];
            command.extend(
                arguments
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_string),
            );
            command
        })
        .or_else(|| {
            ["script_body", "cmd", "command"]
                .into_iter()
                .find_map(|field| match arguments.get(field) {
                    Some(JsonValue::String(command)) => Some(vec![command.clone()]),
                    Some(JsonValue::Array(command)) => command
                        .iter()
                        .map(|part| part.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>(),
                    _ => None,
                })
        })
}

/// A patch envelope routed through a shell wrapper spawns a process, hides the
/// patch exit status behind the wrapper, and bypasses the native apply_patch
/// interception. When the typed tool is registered, fail fast with the exact
/// call form instead of running the wrapper.
fn wrapped_patch_rejection(
    tool_name: &ToolName,
    payload: &ToolPayload,
    apply_patch_available: bool,
) -> Option<String> {
    if !apply_patch_available {
        return None;
    }
    let command = nested_command_argv(tool_name, payload)?;
    if !command
        .iter()
        .any(|part| part.contains(APPLY_PATCH_ENVELOPE_MARKER))
        || is_native_apply_patch_invocation(&command)
        || !is_wrapped_apply_patch_invocation(&command)
    {
        return None;
    }
    Some(format!(
        "Patch envelopes must go through the apply_patch tool, not a shell wrapper: call `await tools.apply_patch(patch)` with the same `{APPLY_PATCH_ENVELOPE_MARKER}` body. The wrapped `{}` command was not run.",
        tool_name.name
    ))
}

fn is_wrapped_apply_patch_invocation(command: &[String]) -> bool {
    let script = match command {
        [script] => script.as_str(),
        _ => match codex_shell_command::parse_command::extract_shell_command(command) {
            Some((_, script)) => script,
            None => return false,
        },
    };
    // Recognize the complete PowerShell literal-pipeline shape. Marker-bearing
    // searches, printed examples, and ambiguous scripts use ordinary dispatch.
    let script = script.trim();
    for (opening, closing) in [("@'", "'@"), ("@\"", "\"@")] {
        if let Some(body) = script.strip_prefix(opening).and_then(|body| {
            body.strip_prefix("\r\n")
                .or_else(|| body.strip_prefix('\n'))
        }) && let Some((_, tail)) = body.split_once(&format!("\n{closing}"))
        {
            return matches!(
                tail.trim().strip_prefix('|').map(str::trim),
                Some("apply_patch" | "applypatch")
            );
        }
    }
    codex_shell_command::bash::try_parse_shell(script)
        .and_then(|tree| {
            codex_shell_command::bash::try_parse_word_only_commands_sequence(&tree, script)
        })
        .is_some_and(|commands| {
            commands.iter().any(|argv| {
                matches!(
                    argv.first().map(String::as_str),
                    Some("apply_patch" | "applypatch")
                )
            })
        })
}

/// Plain `apply_patch` heredoc forms are intercepted natively without a
/// process spawn; only wrapped or piped envelopes are rejected.
fn is_native_apply_patch_invocation(command: &[String]) -> bool {
    fn script_starts_with_apply_patch(script: &str) -> bool {
        let script = script.trim_start();
        script
            .strip_prefix("apply_patch")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    }
    fn program_stem(program: &str) -> String {
        if program.chars().any(char::is_whitespace) {
            return String::new();
        }
        std::path::Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase()
    }
    match command {
        [script] => script_starts_with_apply_patch(script),
        [program, ..] if program_stem(program) == "apply_patch" => true,
        [shell, flag, script]
            if matches!(program_stem(shell).as_str(), "bash" | "sh" | "zsh")
                && matches!(flag.as_str(), "-lc" | "-c") =>
        {
            script_starts_with_apply_patch(script)
        }
        _ => false,
    }
}

fn is_batchable_observation(tool_name: &ToolName, payload: &ToolPayload) -> bool {
    if tool_name.namespace.is_none()
        && matches!(tool_name.name.as_str(), "read_tool_output" | "tool_search")
    {
        return true;
    }
    if tool_name.namespace.is_some()
        || !matches!(
            tool_name.name.as_str(),
            "exec_command" | "shell_command" | "unified_exec"
        )
    {
        return false;
    }
    let ToolPayload::Function { arguments } = payload else {
        return false;
    };
    let Ok(arguments) = serde_json::from_str::<JsonValue>(arguments) else {
        return false;
    };
    let command = arguments
        .get("program")
        .and_then(JsonValue::as_str)
        .map(|program| {
            let mut command = vec![program.to_string()];
            command.extend(
                arguments
                    .get("args")
                    .and_then(JsonValue::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(JsonValue::as_str)
                    .map(str::to_string),
            );
            command
        })
        .or_else(|| {
            ["script_body", "cmd", "command"]
                .into_iter()
                .find_map(|field| match arguments.get(field) {
                    Some(JsonValue::String(command)) => Some(vec![command.clone()]),
                    Some(JsonValue::Array(command)) => command
                        .iter()
                        .map(|part| part.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>(),
                    _ => None,
                })
        });
    command.is_some_and(|command| !crate::turn_diff_tracker::command_may_mutate(&command))
}

fn result_has_live_exec_session(result: &JsonValue) -> bool {
    [
        (
            result.get("session_id"),
            result.get("process_exited"),
            result.get("exit_code"),
        ),
        (
            result.pointer("/result/essential/session_id"),
            result.pointer("/result/essential/process_exited"),
            result.pointer("/result/essential/exit_code"),
        ),
    ]
    .into_iter()
    .any(|(session_id, process_exited, exit_code)| {
        session_id.is_some_and(|session_id| !session_id.is_null())
            && !process_exited.and_then(JsonValue::as_bool).unwrap_or(false)
            && exit_code.is_none_or(JsonValue::is_null)
    })
}

fn nested_command_state(
    tool_name: Option<&ToolName>,
    call_id: &str,
    payload: &ToolPayload,
    result: &JsonValue,
) -> Option<JsonValue> {
    let tool_name = tool_name?;
    let mut state = command_result_state(result)?;
    state["call_id"] = call_id.into();
    state["tool"] = tool_name.name.clone().into();
    if let ToolPayload::Function { arguments } = payload
        && let Ok(arguments) = serde_json::from_str::<JsonValue>(arguments)
    {
        if tool_name.name == "write_stdin" {
            state["polled_session_id"] = arguments["session_id"].clone();
        }
        // Display intent is not a claim that the script consumed the result.
        if arguments["max_output_tokens"] == 0
            && result["streams_complete"] == true && result["exit_code"] == 0
            && result["process_exited"] == true && result["session_id"].is_null()
            && result["error"].is_null()
        {
            state["display_suppressed"] = true.into();
        }
    }
    Some(state)
}

/// Exact command controls shared by code-mode and history pressure receipts.
pub(crate) fn command_result_state(result: &JsonValue) -> Option<JsonValue> {
    // Command capability comes from the registered runtime, not a public-name
    // whitelist. Legacy shell forwarding returns the unified owner's contract.
    if result.get("process_exited").is_none() {
        return None;
    }
    let mut state = serde_json::json!({});
    for key in [
        "error",
        "chunk_id",
        "session_id",
        "exit_code",
        "execution_state",
        "session_capabilities",
        "process_exited",
        "search_no_match",
        "pending_deferred_completions",
        "output_complete",
        "output_reduced",
        "streams_complete",
        "raw_output_artifact_id",
        "raw_output_artifact_bytes",
        "raw_output_artifact_error",
        "raw_output_artifact_retention_limit_hit",
        "raw_output_artifact_retention_limit_reason",
    ] {
        if let Some(value) = result.get(key) {
            state[key] = value.clone();
        }
    }
    if let Some(session_id) = result.get("session_id").filter(|id| id.as_u64().is_some())
        && let Some(incarnation) = result.pointer("/session_capabilities/incarnation").and_then(JsonValue::as_str)
    {
        state["continuation"] = serde_json::json!({
            "tool": "write_stdin", "arguments": {"session_id": session_id, "incarnation": incarnation, "chars": ""}
        });
    }
    if let Some(artifact_id) = result.get("raw_output_artifact_id")
        && let Some(selector) = result.get("recovery_selector")
    {
        state["recovery"] = serde_json::json!({
            "tool": "read_tool_output", "arguments": {
                "artifact_id": artifact_id,
                "selectors": [selector]
            }
        });
    }
    Some(state)
}

fn command_state_session(state: &JsonValue) -> Option<&JsonValue> {
    state
        .get("polled_session_id")
        .filter(|value| !value.is_null())
        .or_else(|| state.get("session_id").filter(|value| !value.is_null()))
}

fn nested_failure_fingerprint(tool_name: &ToolName, error: &str) -> String {
    if let Some(json_start) = error.find('{')
        && let Ok(value) = serde_json::from_str::<JsonValue>(&error[json_start..])
        && let Some(fingerprint) = value
            .get("fingerprint")
            .or_else(|| {
                value
                    .get("failure")
                    .and_then(|failure| failure.get("fingerprint"))
            })
            .and_then(JsonValue::as_str)
            .filter(|fingerprint| !fingerprint.is_empty())
    {
        return fingerprint.to_string();
    }
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "code_mode.nested_tool.{:x}",
        Sha256::digest(format!("{tool_name}\0{normalized}").as_bytes())
    )
}

fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function
            if tool_name.namespace.is_none()
                && tool_name.name == codex_tools::TOOL_SEARCH_TOOL_NAME =>
        {
            build_tool_search_payload(tool_name, input)
        }
        CodeModeToolKind::Function => build_function_tool_payload(tool_name, input),
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn build_tool_search_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let input = match input {
        None => serde_json::json!({}),
        Some(input @ JsonValue::Object(_)) => input,
        Some(_) => {
            return Err(format!(
                "tool `{tool_name}` expects a JSON object for arguments"
            ));
        }
    };
    let arguments = serde_json::from_value(input)
        .map_err(|err| format!("failed to parse tool `{tool_name}` arguments: {err}"))?;
    Ok(ToolPayload::ToolSearch { arguments })
}

fn build_function_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    Ok(ToolPayload::Function { arguments })
}

fn serialize_function_tool_arguments(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<String, String> {
    match input {
        None => Ok("{}".to_string()),
        Some(JsonValue::Object(map)) => serde_json::to_string(&JsonValue::Object(map))
            .map_err(|err| format!("failed to serialize tool `{tool_name}` arguments: {err}")),
        Some(_) => Err(format!(
            "tool `{tool_name}` expects a JSON object for arguments"
        )),
    }
}

fn build_freeform_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match input {
        Some(JsonValue::String(input)) => Ok(ToolPayload::Custom { input }),
        _ => Err(format!("tool `{tool_name}` expects a string input")),
    }
}

#[cfg(test)]
#[path = "response_tests.rs"]
mod response_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CodeModeService;
    use super::MAX_RETAINED_NESTED_RESULT_BYTES;
    use super::OutputOutcome;
    use super::bounded_serialized_json;
    use super::build_nested_tool_payload;
    use super::fold_nested_required_terminal;
    use super::format_runtime_response;
    use super::is_batchable_observation;
    use super::nested_failure_fingerprint;
    use super::required_nested_tool_terminal_cause;
    use super::result_has_live_exec_session;
    use super::truncate_code_mode_result;
    use super::wrapped_patch_rejection;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::RequiredToolTerminalCause;
    use crate::tools::context::ToolPayload;
    use codex_code_mode::CellId;
    use codex_code_mode::CodeModeToolKind;
    use codex_code_mode::ExecuteRequest;
    use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
    use codex_code_mode::RuntimeResponse;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::SearchToolCallParams;
    use codex_tools::ToolName;
    use codex_tools::ToolOutput;
    use codex_tools::ToolOutputOutcome;
    use codex_tools::ToolOutputOutcomeContext;
    use codex_tools::ToolOutputSkipDisposition;
    use serde_json::json;

    fn test_service() -> CodeModeService {
        CodeModeService::new(Arc::new(ProcessOwnedCodeModeSessionProvider::default()))
    }

    #[tokio::test]
    async fn completion_boundary_recovery_is_independent_of_empty_yields() {
        let (_session, turn) = crate::session::tests::make_session_and_context().await;
        for yielded in [false, true] {
            let service = test_service();
            let cell = CellId::new("recovery-boundary".into());
            let (activity, receiver) = tokio::sync::watch::channel(crate::session::InputQueueActivity::InternalCompletion);
            service.record_cell_parent_call_id(&cell, "outer");
            service.record_delivery_intent(&cell, &turn, receiver.clone());
            let recovery = service.record_delivery_evidence(&cell, true,
                Some(("artifact".into(), "hash".into(), 12)), None, &json!({"preview":"partial"}));
            assert_eq!(recovery.unwrap()["arguments"]["artifact_id"], "artifact");
            if yielded { service.finish_packet(cell.as_str(), false); }
            let response = RuntimeResponse::Result {
                cell_id: cell.clone(), error_text: None, output_loss: None,
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText { text:"answer".into() }],
            };
            assert_eq!(service.delivery_for_response(&cell, &turn, &response).unwrap_err()["category"], "evidence_recovery_pending");
            for (hash, ranges) in [("wrong", json!([[0,12]])), ("hash", json!([[0,5]])), ("hash", json!([[5,12]]))] {
                service.record_delivery_evidence(&cell, false, None,
                    Some(&json!({"semantic_evidence":{"source":"artifact","scope":"artifact",
                        "identity":{"sha256":hash,"ranges":ranges,"values":[]}}})), &json!({}));
                if yielded { service.finish_packet(cell.as_str(), false); }
            }
            service.record_delivery_intent(&cell, &turn, receiver);
            assert_eq!(service.delivery_for_response(&cell, &turn, &response).unwrap(), Some("answer".into()));
            drop(activity);
        }
    }

    #[test]
    fn completion_boundary_storage_only_output_does_not_require_recovery() {
        let service = test_service();
        let cell = CellId::new("storage-only".into());
        service.record_cell_parent_call_id(&cell, "outer");
        let raw = json!({"answer":42});
        let canonical = codex_tools::CanonicalToolResult::json(raw.clone());
        assert!(service.record_delivery_evidence(&cell, true,
            Some(("artifact".into(), canonical.sha256, canonical.exact_bytes)), None, &raw).is_none());
        assert!(service.packet_admission.lock().unwrap().cells["storage-only"].delivery_recovery.is_empty());
    }

    #[tokio::test]
    async fn uncertainty_recovery_survives_cell_close_without_gating_unrelated_turns() {
        let (_session, turn) = crate::session::tests::make_session_and_context().await;
        let service = test_service();
        let first = CellId::new("closed-evidence".into());
        service.record_cell_parent_call_id(&first, "first");
        service.record_cell_turn(&first, &turn.sub_id);
        service.record_delivery_evidence(&first, true,
            Some(("artifact".into(), "hash".into(), 12)), None, &json!({"preview":"partial"}));
        service.finish_packet(first.as_str(), false);
        service.finish_cell_dispatch(&first);
        assert_eq!(service.packet_admission.lock().unwrap().closed_recovery.len(), 1);
        let second = CellId::new("consume-evidence".into());
        service.record_cell_parent_call_id(&second, "second");
        let (_activity, receiver) = tokio::sync::watch::channel(crate::session::InputQueueActivity::InternalCompletion);
        let response = RuntimeResponse::Result { cell_id: second.clone(), error_text: None, output_loss: None,
            content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText { text: "answer".into() }] };
        service.record_delivery_intent(&second, &turn, receiver.clone());
        assert_eq!(service.delivery_for_response(&second, &turn, &response).unwrap_err()["category"], "evidence_recovery_pending");
        for (hash, ranges, pending) in [("wrong", json!([[0,12]]), true),
            ("hash", json!([[0,5]]), true), ("hash", json!([[5,12]]), false)] {
            service.record_delivery_evidence(&second, false, None,
                Some(&json!({"semantic_evidence":{"source":"artifact","scope":"artifact",
                    "identity":{"sha256":hash,"ranges":ranges,"values":[]}}})), &json!({}));
            assert_eq!(!service.packet_admission.lock().unwrap().closed_recovery.is_empty(), pending);
        }
        service.record_delivery_intent(&second, &turn, receiver.clone());
        assert_eq!(service.delivery_for_response(&second, &turn, &response).unwrap(), Some("answer".into()));
        service.record_cell_turn(&first, "no-resurrection");
        assert!(!service.packet_admission.lock().unwrap().cells.contains_key(first.as_str()));
        // A different task is not forced to recover an earlier task's output.
        service.record_cell_turn(&second, "earlier-turn");
        service.record_delivery_evidence(&second, true,
            Some(("other".into(), "other-hash".into(), 10)), None, &json!({}));
        service.finish_cell_dispatch(&second);
        let third = CellId::new("unrelated-turn".into());
        service.record_cell_parent_call_id(&third, "third");
        service.record_delivery_intent(&third, &turn, receiver);
        assert!(service.delivery_for_response(&third, &turn, &response).unwrap().is_some());
        assert_eq!(service.packet_admission.lock().unwrap().closed_recovery.len(), 1);
    }

    #[test]
    fn paginated_selection_requires_exact_authenticated_coverage() {
        let service = test_service();
        let cell = CellId::new("partial-selection".into());
        service.record_cell_parent_call_id(&cell, "outer");
        service.record_delivery_evidence(&cell, false, None, None, &json!({
            "complete": false, "artifact_id": "artifact", "canonical_sha256": "hash",
            "canonical_bytes": 100, "results": [{"canonical_range": {"start": 20, "end": 40}}]
        }));
        for (hash, ranges, pending) in [
            ("wrong", json!([[20,40]]), true),
            ("hash", json!([[20,30]]), true),
            ("hash", json!([[31,40]]), true),
            ("hash", json!([[30,31]]), false),
        ] {
            service.record_delivery_evidence(&cell, false, None, Some(&json!({
                "semantic_evidence": {"source":"artifact", "scope":"artifact",
                    "identity":{"sha256":hash, "ranges":ranges, "values":[]}}
            })), &json!({}));
            let admission = service.packet_admission.lock().unwrap();
            let metrics = &admission.cells[cell.as_str()];
            assert_eq!(!metrics.delivery_recovery.is_empty(), pending);
            assert!(!metrics.delivery_blocked);
        }
        service.complete_packet_call(&cell, 0, false, 0, Vec::new(), None,
            Some((RequiredToolTerminalCause::Failure, "genuine failure".into())));
        service.record_delivery_evidence(&cell, false, None, None, &json!({"complete":true}));
        assert!(service.packet_admission.lock().unwrap().cells[cell.as_str()].delivery_blocked);
    }

    #[test]
    fn recovery_budget_survives_packet_drain_and_tracks_wait_updates() {
        let service = test_service();
        let cell = CellId::new("budget-cell".to_string());
        service.record_cell_parent_call_id(&cell, "outer");
        service.record_output_budget(&cell, Some(4_000));
        service.finish_packet(cell.as_str(), false);
        assert_eq!(service.output_budget(cell.as_str()), Some(4_000));
        service.record_output_budget(&cell, Some(1_000));
        assert_eq!(service.output_budget(cell.as_str()), Some(1_000));
        service.finish_cell_dispatch(&cell);
        service.record_output_budget(&cell, Some(10_000));
        assert_eq!(service.output_budget(cell.as_str()), None);
    }

    #[test]
    fn retained_nested_result_allocates_in_proportion_to_small_output() {
        let mut writer = super::BoundedJsonWriter::new(MAX_RETAINED_NESTED_RESULT_BYTES);
        serde_json::to_writer(&mut writer, &json!({"ok": true})).unwrap();
        assert!(writer.bytes.capacity() < MAX_RETAINED_NESTED_RESULT_BYTES);
        assert_eq!(writer.finish(), (r#"{"ok":true}"#.to_string(), false, 11));
    }

    #[test]
    fn retained_nested_result_counts_full_json_bytes_including_utf8() {
        let value = json!({
            "text": "multi-byte: é",
            "nested": [true, null, {"count": 17}],
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);
        let expected = r#"{"text":"multi-byte: é","nested":[true,null,{"count":17}]}"#;
        assert_eq!(bytes, expected.len());
        assert!(!truncated);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&retained).unwrap(),
            value
        );
    }

    #[test]
    fn retained_nested_result_serialization_is_bounded() {
        let value = json!({
            "a_prefix": "kept",
            "body": "x".repeat(MAX_RETAINED_NESTED_RESULT_BYTES * 2),
            "tail": "MUST_NOT_BE_RETAINED",
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);
        let expected = format!(
            r#"{{"a_prefix":"kept","body":"{}","tail":"MUST_NOT_BE_RETAINED"}}"#,
            "x".repeat(MAX_RETAINED_NESTED_RESULT_BYTES * 2)
        );

        assert!(truncated);
        assert_eq!(retained, expected[..MAX_RETAINED_NESTED_RESULT_BYTES]);
        assert_eq!(bytes, expected.len());
        assert!(!retained.contains("MUST_NOT_BE_RETAINED"));
    }

    #[test]
    fn retained_nested_result_bound_never_splits_utf8() {
        let value = json!({
            "body": "🙂".repeat(MAX_RETAINED_NESTED_RESULT_BYTES),
        });

        let (retained, truncated, bytes) = bounded_serialized_json(&value);

        assert!(truncated);
        assert_eq!(retained, format!("{{\"body\":\"{}", "🙂".repeat(253)));
        assert_eq!(bytes, 4_107);
    }

    #[test]
    fn cell_parent_call_id_lives_until_dispatch_finishes() {
        let service = test_service();
        let cell = CellId::new("cell-parent".to_string());

        service.record_cell_parent_call_id(&cell, "outer-exec-call");
        assert_eq!(
            service.cell_parent_call_id(&cell).as_deref(),
            Some("outer-exec-call")
        );

        service.finish_cell_dispatch(&cell);
        assert_eq!(service.cell_parent_call_id(&cell), None);
    }

    #[tokio::test]
    async fn prewarm_initializes_the_code_mode_session_once() {
        let service =
            CodeModeService::new(Arc::new(codex_code_mode::InProcessCodeModeSessionProvider));
        assert!(!service.is_initialized());

        service.prewarm().await.expect("in-process host prewarm");
        assert!(service.is_initialized());
        let first = service.session().await.expect("prewarmed session");
        let second = service.session().await.expect("reused session");
        assert!(Arc::ptr_eq(&first, &second));
    }

    struct ShutdownTestSession {
        result: Result<(), String>,
        shutdown_calls: std::sync::Mutex<usize>,
    }

    impl codex_code_mode::CodeModeSession for ShutdownTestSession {
        fn execute<'a>(
            &'a self,
            _request: ExecuteRequest,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell>
        {
            panic!("shutdown must prevent execution");
        }

        fn wait<'a>(
            &'a self,
            _request: codex_code_mode::WaitRequest,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome>
        {
            panic!("shutdown must prevent waiting on cells");
        }

        fn terminate<'a>(
            &'a self,
            _cell_id: CellId,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome>
        {
            panic!("shutdown must prevent cell operations");
        }

        fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
            Box::pin(async move {
                *self.shutdown_calls.lock().unwrap() += 1;
                self.result.clone()
            })
        }
    }

    struct DelayedSessionProvider {
        session: Arc<ShutdownTestSession>,
        finish_creation: tokio::sync::Notify,
        creation_calls: std::sync::Mutex<usize>,
    }

    impl codex_code_mode::CodeModeSessionProvider for DelayedSessionProvider {
        fn create_session<'a>(
            &'a self,
            _delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
        ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
            Box::pin(async move {
                *self.creation_calls.lock().unwrap() += 1;
                self.finish_creation.notified().await;
                Ok(Arc::clone(&self.session) as Arc<dyn codex_code_mode::CodeModeSession>)
            })
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_initialization_and_preserves_cleanup_errors() {
        for cleanup_result in [Ok(()), Err("host cleanup failed".to_string())] {
            let session = Arc::new(ShutdownTestSession {
                result: cleanup_result.clone(),
                shutdown_calls: Default::default(),
            });
            let provider = Arc::new(DelayedSessionProvider {
                session: Arc::clone(&session),
                finish_creation: Default::default(),
                creation_calls: Default::default(),
            });
            let service = CodeModeService::new(provider.clone());
            let initialization = service.prewarm();
            tokio::pin!(initialization);
            assert!(futures::poll!(initialization.as_mut()).is_pending());
            assert_eq!(*provider.creation_calls.lock().unwrap(), 1);

            let shutdown = service.shutdown();
            tokio::pin!(shutdown);
            assert!(futures::poll!(shutdown.as_mut()).is_pending());
            assert_eq!(*session.shutdown_calls.lock().unwrap(), 0);
            provider.finish_creation.notify_one();

            let (initialization_result, shutdown_result) = tokio::join!(initialization, shutdown);
            assert_eq!(
                initialization_result,
                Err("code mode session is shutting down".to_string())
            );
            assert_eq!(shutdown_result, cleanup_result);
            assert_eq!(*session.shutdown_calls.lock().unwrap(), 1);
            assert_eq!(*provider.creation_calls.lock().unwrap(), 1);
            assert_eq!(
                service.prewarm().await,
                Err("code mode session is shutting down".to_string())
            );
        }
    }

    #[tokio::test]
    async fn shutdown_does_not_initialize_an_unused_service() {
        let session = Arc::new(ShutdownTestSession {
            result: Ok(()),
            shutdown_calls: Default::default(),
        });
        let provider = Arc::new(DelayedSessionProvider {
            session: Arc::clone(&session),
            finish_creation: Default::default(),
            creation_calls: Default::default(),
        });
        let service = CodeModeService::new(provider.clone());

        assert_eq!(service.shutdown().await, Ok(()));
        assert_eq!(
            service.prewarm().await,
            Err("code mode session is shutting down".to_string())
        );
        assert_eq!(*provider.creation_calls.lock().unwrap(), 0);
        assert_eq!(*session.shutdown_calls.lock().unwrap(), 0);
    }

    #[test]
    fn wrapped_patch_envelopes_are_rejected_only_when_apply_patch_is_registered() {
        let exec_command = ToolName::plain("exec_command");
        let envelope = "*** Begin Patch\n*** Update File: a.py\n*** End Patch";
        let wrapped = ToolPayload::Function {
            arguments: json!({ "cmd": format!("@'\n{envelope}\n'@ | apply_patch") }).to_string(),
        };
        let rejection = wrapped_patch_rejection(&exec_command, &wrapped, true)
            .expect("a piped envelope must be rejected");
        assert!(rejection.contains("await tools.apply_patch(patch)"));
        assert!(rejection.contains("was not run"));
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &wrapped, false),
            None
        );

        let lt = '<';
        let heredoc = format!("apply_patch {lt}{lt}'EOF'\n{envelope}\nEOF");
        let native = ToolPayload::Function {
            arguments: json!({ "cmd": heredoc }).to_string(),
        };
        assert_eq!(wrapped_patch_rejection(&exec_command, &native, true), None);
        let native_argv = ToolPayload::Function {
            arguments: json!({ "program": "bash", "args": ["-lc", heredoc] }).to_string(),
        };
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &native_argv, true),
            None
        );
        let ordinary = ToolPayload::Function {
            arguments: json!({ "cmd": "git diff" }).to_string(),
        };
        assert_eq!(
            wrapped_patch_rejection(&exec_command, &ordinary, true),
            None
        );
        assert_eq!(
            wrapped_patch_rejection(&ToolName::plain("read_tool_output"), &wrapped, true),
            None
        );
        for cmd in [
            r#"rg --fixed-strings "*** Begin Patch" src"#,
            r#"printf '%s' '*** Begin Patch | apply_patch'"#,
            r#"Write-Output '*** Begin Patch | apply_patch'"#,
            "@'\n*** Begin Patch\n'@ | Write-Output",
        ] {
            let payload = ToolPayload::Function {
                arguments: json!({ "cmd": cmd }).to_string(),
            };
            assert_eq!(
                wrapped_patch_rejection(&exec_command, &payload, true),
                None,
                "{cmd}"
            );
        }
        let piped = ToolPayload::Function {
            arguments: json!({ "cmd": format!("printf '%s' '{envelope}' | apply_patch") })
                .to_string(),
        };
        assert!(wrapped_patch_rejection(&exec_command, &piped, true).is_some());
    }

    /// Builds a real session, step context, and tool runtime with the given
    /// registered tools, exactly as the turn loop does for nested calls.
    async fn nested_call_fixture(
        tools: Vec<Arc<dyn crate::tools::registry::CoreToolRuntime>>,
    ) -> (
        Arc<crate::session::session::Session>,
        Arc<crate::session::turn_context::TurnContext>,
        crate::tools::parallel::ToolCallRuntime,
    ) {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        turn.approval_policy
            .set(codex_protocol::protocol::AskForApproval::Never)
            .unwrap();
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let step_context = crate::session::step_context::StepContext::for_test(Arc::clone(&turn));
        let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
            crate::tools::registry::ToolRegistry::from_tools(tools),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        ));
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            Arc::clone(&session),
            step_context,
            tracker,
        );
        (session, turn, runtime)
    }

    fn wrapped_patch_invocation(cell_id: &CellId) -> codex_code_mode::CodeModeNestedToolCall {
        let envelope = "*** Begin Patch\n*** Update File: a.py\n*** End Patch";
        codex_code_mode::CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            parent_tool_call_id: Some("outer-exec".to_string()),
            runtime_tool_call_id: "runtime-call-1".to_string(),
            tool_name: ToolName::plain("exec_command"),
            tool_kind: CodeModeToolKind::Function,
            input: Some(json!({ "cmd": format!("@'\n{envelope}\n'@ | apply_patch") })),
            nested_deadline: None,
            buffered_output_bytes: 0,
        }
    }

    #[tokio::test]
    async fn wrapped_patch_through_a_real_nested_exec_command_is_rejected_before_dispatch() {
        let apply_patch: Arc<dyn crate::tools::registry::CoreToolRuntime> = Arc::new(
            crate::tools::handlers::ApplyPatchHandler::new(/*multi_environment*/ false),
        );
        let (session, turn, runtime) = nested_call_fixture(vec![apply_patch]).await;
        let cell_id = CellId::new("cell-patch".to_string());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-exec");
        let exec = super::ExecContext {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
        };

        let result = super::call_nested_tool(
            exec,
            runtime,
            wrapped_patch_invocation(&cell_id),
            codex_code_mode::NestedCancellation::new(tokio_util::sync::CancellationToken::new()),
        )
        .await;

        let Err(crate::FunctionCallError::RespondToModel(message)) = result else {
            panic!("a wrapped patch must be rejected before dispatch, got {result:?}");
        };
        assert!(message.contains("await tools.apply_patch(patch)"));
        assert!(message.contains("was not run"));
        let packet = session
            .services
            .code_mode_service
            .finish_packet(cell_id.as_str(), false);
        assert_eq!(packet.nested_call_count, 1);
        let terminal = packet
            .first_required_terminal
            .expect("the rejection is recorded as the cell's required terminal failure");
        assert_eq!(terminal.cause, RequiredToolTerminalCause::Failure);
        assert!(terminal.message.contains("was not run"));
    }

    #[tokio::test]
    async fn earlier_prints_do_not_erase_later_nested_command_results() {
        let handler: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ExecCommandHandler::default());
        let (session, turn, runtime) = nested_call_fixture(vec![handler]).await;
        for (index, (buffered, cap)) in [(0, 1024), (200_000, 1024), (200_000, 0)]
            .into_iter().enumerate()
        {
            let cell_id = CellId::new(format!("buffered-command-{index}"));
            let service = &session.services.code_mode_service;
            service.record_cell_parent_call_id(&cell_id, "outer-command");
            let result = super::call_nested_tool(
                super::ExecContext { session: Arc::clone(&session), turn: Arc::clone(&turn) },
                runtime.clone(),
                codex_code_mode::CodeModeNestedToolCall {
                    cell_id: cell_id.clone(),
                    parent_tool_call_id: Some("outer-command".into()),
                    runtime_tool_call_id: format!("command-{index}"),
                    tool_name: ToolName::plain("exec_command"),
                    tool_kind: CodeModeToolKind::Function,
                    input: Some(json!({
                        "cmd": "echo retained-command-evidence", "max_output_tokens": cap,
                        "yield_time_ms": 30000,
                    })),
                    nested_deadline: None,
                    buffered_output_bytes: buffered,
                },
                codex_code_mode::NestedCancellation::new(tokio_util::sync::CancellationToken::new()),
            ).await.unwrap();
            assert_eq!(result["exit_code"], 0, "{result}");
            assert_eq!(result["process_exited"], true);
            assert_eq!(result["streams_complete"], true);
            assert_eq!(result["stdout"].as_str().unwrap().trim(), "retained-command-evidence");
            assert_eq!(result["output"].as_str().unwrap().trim(),
                if cap == 0 { "" } else { "retained-command-evidence" });
            assert_eq!(result["output_reduced"], cap == 0);
            if cap != 0 {
                assert!(result.get("raw_output_artifact_id").is_none(),
                    "small intact results should not require an extra artifact");
            }
            let packet = service.finish_packet(cell_id.as_str(), false);
            assert_eq!(packet.nested_call_count, 1);
            assert!(packet.first_required_terminal.is_none());
            service.finish_cell_dispatch(&cell_id);
        }
    }

    #[tokio::test]
    async fn harmless_patch_marker_reaches_the_registered_shell_handler() {
        let apply_patch: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ApplyPatchHandler::new(false));
        let shell: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ShellCommandHandler::new(
                crate::tools::handlers::ShellCommandHandlerOptions {
                    foreign_environment: false,
                    allow_login_shell: false,
                    allow_escalated_sandbox_permissions: false,
                    exec_permission_approvals_enabled: false,
                },
            ));
        let (session, turn, runtime) = nested_call_fixture(vec![apply_patch, shell]).await;
        let cell_id = CellId::new("harmless-patch-marker".to_string());
        let service = &session.services.code_mode_service;
        service.record_cell_parent_call_id(&cell_id, "outer-exec");
        let result = super::call_nested_tool(
            super::ExecContext {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn),
            },
            runtime,
            codex_code_mode::CodeModeNestedToolCall {
                cell_id: cell_id.clone(),
                parent_tool_call_id: Some("outer-exec".to_string()),
                runtime_tool_call_id: "print-marker".to_string(),
                tool_name: ToolName::plain("shell_command"),
                tool_kind: CodeModeToolKind::Function,
                input: Some(json!({ "command": "echo '*** Begin Patch'" })),
                nested_deadline: None,
                buffered_output_bytes: 0,
            },
            codex_code_mode::NestedCancellation::new(tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect("printing patch-shaped data must reach ordinary shell dispatch");
        assert!(result.to_string().contains("*** Begin Patch"));
        let packet = service.finish_packet(cell_id.as_str(), false);
        assert_eq!(packet.nested_call_count, 1);
        assert!(packet.first_required_terminal.is_none());
        service.finish_cell_dispatch(&cell_id);
    }

    #[tokio::test]
    async fn large_nested_source_read_retains_a_source_scoped_recoverable_artifact() {
        let shell: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(crate::tools::handlers::ShellCommandHandler::new(
                crate::tools::handlers::ShellCommandHandlerOptions {
                    foreign_environment: false,
                    allow_login_shell: false,
                    allow_escalated_sandbox_permissions: false,
                    exec_permission_approvals_enabled: false,
                },
            ));
        let (session, turn, runtime) = nested_call_fixture(vec![shell]).await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.txt");
        let contents = "exact independent source line\n".repeat(300);
        std::fs::write(&path, &contents).unwrap();
        let command = if cfg!(windows) {
            format!("Get-Content -LiteralPath '{}' -Raw", path.display())
        } else {
            format!("cat '{}'", path.display())
        };
        let cell_id = CellId::new("large-source-cell".into());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-source");
        let output = super::call_nested_tool(
            super::ExecContext {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn),
            },
            runtime,
            codex_code_mode::CodeModeNestedToolCall {
                cell_id: cell_id.clone(),
                parent_tool_call_id: Some("outer-source".into()),
                runtime_tool_call_id: "read-source".into(),
                tool_name: ToolName::plain("shell_command"),
                tool_kind: CodeModeToolKind::Function,
                input: Some(json!({"command":command})),
                nested_deadline: None,
                buffered_output_bytes: 0,
            },
            codex_code_mode::NestedCancellation::new(tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap();
        assert!(output.to_string().len() > 4_096);
        let history = session
            .lock_history_state_for_test()
            .await
            .tool_history_state();
        let ledger = serde_json::to_value(&history).unwrap();
        let nested = ledger["code_mode_nested_evidence"]["outer-source"]
            .as_object()
            .expect("retain independent source");
        assert_eq!(nested.len(), 1);
        let evidence = nested.values().next().unwrap();
        assert!(
            !evidence["observation"]["source_dependencies"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let pin: serde_json::Value =
            serde_json::from_str(evidence["output"].as_str().unwrap()).unwrap();
        let artifact_id = pin["artifact_id"].as_str().unwrap();
        assert!(history.artifact_references().contains_key(artifact_id));
        let exact = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            artifact_id,
            100_000,
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&exact).unwrap(),
            output
        );
        session
            .services
            .code_mode_service
            .finish_cell_dispatch(&cell_id);
    }

    #[tokio::test]
    async fn wrapped_patch_is_not_rejected_when_no_apply_patch_tool_is_registered() {
        let (session, turn, runtime) = nested_call_fixture(Vec::new()).await;
        let cell_id = CellId::new("cell-no-patch-tool".to_string());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-exec");
        let exec = super::ExecContext {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
        };

        let result = super::call_nested_tool(
            exec,
            runtime,
            wrapped_patch_invocation(&cell_id),
            codex_code_mode::NestedCancellation::new(tokio_util::sync::CancellationToken::new()),
        )
        .await;

        // Without a typed patch tool the call reaches ordinary dispatch, which
        // fails here only because this fixture registers no exec_command tool.
        let Err(error) = result else {
            panic!("dispatch without a registered exec_command tool must fail");
        };
        assert!(
            !error.to_string().contains("await tools.apply_patch(patch)"),
            "the wrapped-patch rejection must depend on apply_patch being registered: {error}"
        );
    }

    #[test]
    fn packet_metrics_are_drained_between_responses() {
        let service = test_service();
        let cell = CellId::new("cell".to_string());
        service.record_packet_call(&cell, true, 128, Vec::new());
        let first = service.finish_packet("cell", false);
        assert_eq!(first.nested_call_count, 1);
        assert_eq!(first.batchable_observation_count, 1);
        assert_eq!(first.result_bytes, 128);
        let drained = service.finish_packet("cell", false);
        assert_eq!(drained.nested_call_count, 0);
        assert_eq!(drained.result_bytes, 0);
        for _ in 0..6 {
            service.record_packet_call(&cell, true, 128, Vec::new());
        }
        let batched = service.finish_packet("cell", false);
        assert_eq!(batched.nested_call_count, 6);
        assert_eq!(batched.result_bytes, 768);
    }

    #[test]
    fn post_tool_feedback_is_drained_once_with_code_mode_packet() {
        let service = test_service();
        let cell = CellId::new("feedback-cell".to_string());
        let feedback = vec![FunctionCallOutputContentItem::InputText {
            text: "hook feedback".to_string(),
        }];

        service.record_packet_call(&cell, false, 32, feedback.clone());
        assert_eq!(
            service
                .finish_packet("feedback-cell", false)
                .post_tool_use_feedback,
            feedback
        );
        assert!(
            service
                .finish_packet("feedback-cell", false)
                .post_tool_use_feedback
                .is_empty()
        );
    }

    #[test]
    fn late_failure_displaces_success_in_bounded_packet_evidence() {
        let service = test_service();
        let cell = CellId::new("late-failure-cell".to_string());
        service.record_cell_parent_call_id(&cell, "outer-exec");
        for index in 0..10 {
            let ordinal = service.begin_packet_call(&cell).unwrap();
            service.complete_packet_call(
                &cell,
                ordinal,
                false,
                10,
                Vec::new(),
                Some(super::CodeModeNestedResultEvidence {
                    failed: index == 9,
                    command_state: None,
                    output_fingerprints: Vec::new(),
                    ordinal,
                    call_id: format!("call-{index}"),
                    parent_call_id: Some("outer-exec".into()),
                    parent_cell_id: cell.to_string(),
                    runtime_tool_call_id: index.to_string(),
                    tool_name: "test".into(),
                    output: if index == 9 {
                        "late failure"
                    } else {
                        "success"
                    }
                    .into(),
                    output_truncated: false,
                }),
                None,
            );
        }
        let packet = service.finish_packet("late-failure-cell", false);
        assert_eq!(packet.nested_results.len(), 2);
        assert_eq!(packet.omitted_nested_result_count, 8);
        assert_eq!(packet.nested_recovery.len(), 10);
        for (index, result) in packet.nested_recovery.values().enumerate() {
            assert_eq!(result["call_id"], format!("call-{index}"));
            assert_eq!(result["output"], if index == 9 { "late failure" } else { "success" });
            assert_eq!(result["recovery_available"], true);
        }
        assert!(packet.nested_results.iter().any(|result| result.failed
            && result.call_id == "call-9"
            && result.output == "late failure"));
        assert!(
            packet.first_required_terminal.is_none(),
            "diagnostics must not turn a handled error into a sticky failure"
        );
    }

    #[test]
    fn nested_terminal_fold_uses_registration_order_and_preserves_blocked_status() {
        let service = test_service();
        let cell = CellId::new("terminal-cell".to_string());
        service.record_cell_parent_call_id(&cell, "outer-exec");
        let first = service.begin_packet_call(&cell).unwrap();
        let second = service.begin_packet_call(&cell).unwrap();

        service.complete_packet_call(
            &cell,
            second,
            false,
            0,
            Vec::new(),
            None,
            Some((
                RequiredToolTerminalCause::Failure,
                "second nested failure".to_string(),
            )),
        );
        service.complete_packet_call(
            &cell,
            first,
            false,
            0,
            Vec::new(),
            None,
            Some((
                RequiredToolTerminalCause::Blocked,
                "first nested block".to_string(),
            )),
        );

        let terminal = service
            .finish_packet("terminal-cell", false)
            .first_required_terminal
            .expect("the first registered terminal nested call must be retained");
        assert_eq!(terminal.ordinal, first);
        assert_eq!(terminal.cause, RequiredToolTerminalCause::Blocked);

        let output = fold_nested_required_terminal(
            FunctionToolOutput::from_text("script completed".to_string(), Some(true)),
            terminal,
        );
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Skipped);
        assert_eq!(
            output.skip_disposition,
            Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
        );
        assert_eq!(
            output.sampling_request_signal().unwrap()["outcome"],
            "blocked"
        );
    }

    #[test]
    fn nested_terminal_classification_preserves_recoverable_failures() {
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                None,
            ),
            None,
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(&json!({ "outcome": "blocked" })),
            ),
            Some(RequiredToolTerminalCause::Blocked),
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None,
            ),
            None,
        );
        assert_eq!(
            required_nested_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
                None,
            ),
            None,
        );
    }

    #[test]
    fn packet_admission_only_classifies_known_read_only_argv_calls() {
        let rg = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "rg", "args": ["needle"]}).to_string(),
        };
        let git_status = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "git", "args": ["status", "--short"]})
                .to_string(),
        };
        let git_commit = ToolPayload::Function {
            arguments: json!({"kind": "argv", "program": "git", "args": ["commit"]}).to_string(),
        };
        let read_only_shell_script = ToolPayload::Function {
            arguments: json!({"kind": "script", "cmd": "rg needle"}).to_string(),
        };
        let mutating_shell_script = ToolPayload::Function {
            arguments: json!({"command": "Remove-Item output.txt"}).to_string(),
        };

        assert!(is_batchable_observation(
            &ToolName::plain("exec_command"),
            &rg
        ));
        assert!(is_batchable_observation(
            &ToolName::plain("exec_command"),
            &git_status
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("exec_command"),
            &git_commit
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("exec_command"),
            &read_only_shell_script
        ));
        assert!(!is_batchable_observation(
            &ToolName::plain("shell_command"),
            &mutating_shell_script
        ));
    }

    #[test]
    fn tool_result_correctness_projected_live_exec_session_is_detected() {
        assert!(result_has_live_exec_session(&json!({"session_id": 7})));
        assert!(result_has_live_exec_session(&json!({
            "version": 1,
            "result": {
                "essential": {
                    "session_id": 7,
                },
                "selected_text": "",
            },
        })));
        assert!(!result_has_live_exec_session(&json!({
            "result": {
                "essential": {
                    "session_id": null,
                },
            },
        })));
        assert!(!result_has_live_exec_session(&json!({"exit_code": 0})));
        assert!(!result_has_live_exec_session(&json!({
            "session_id": 7,
            "exit_code": 7,
            "process_exited": true,
        })));
        assert!(!result_has_live_exec_session(&json!({
            "version": 1,
            "result": {
                "essential": {
                    "session_id": 7,
                    "exit_code": null,
                    "process_exited": true,
                },
            },
        })));
    }

    #[test]
    fn build_nested_tool_payload_uses_function_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("example"),
            Some(json!({ "value": 1 })),
        )
        .expect("function payload should serialize");

        match payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, r#"{"value":1}"#.to_string());
            }
            other => panic!("expected function payload, got {other:?}"),
        }
    }

    #[test]
    fn transported_nested_tool_input_preserves_dispatch_validation() {
        use codex_code_mode::CodeModeNestedToolCall;
        use codex_code_mode::host::WireNestedToolCall;

        for (input, expected) in [
            (
                None,
                Ok(ToolPayload::Function {
                    arguments: "{}".to_string(),
                }),
            ),
            (
                Some(json!(null)),
                Err("tool `example` expects a JSON object for arguments".to_string()),
            ),
            (
                Some(json!({ "value": null })),
                Ok(ToolPayload::Function {
                    arguments: r#"{"value":null}"#.to_string(),
                }),
            ),
            (
                Some(json!({"value":9_007_199_254_740_993_u64})),
                Ok(ToolPayload::Function {
                    arguments:r#"{"value":9007199254740993}"#.to_string(),
                }),
            ),
        ] {
            let invocation = CodeModeNestedToolCall {
                cell_id: codex_code_mode::CellId::new("cell-input".to_string()),
                parent_tool_call_id: None,
                runtime_tool_call_id: "nested-call".to_string(),
                tool_name: ToolName::plain("example"),
                tool_kind: CodeModeToolKind::Function,
                input,
                nested_deadline: None,
                buffered_output_bytes: 0,
            };
            let runtime_json = serde_json::to_vec(&invocation).expect("encode runtime input");
            let runtime: CodeModeNestedToolCall =
                serde_json::from_slice(&runtime_json).expect("decode runtime input");
            let wire = WireNestedToolCall::from(invocation.clone());
            let wire_json = serde_json::to_vec(&wire).expect("encode host input");
            let wire: WireNestedToolCall =
                serde_json::from_slice(&wire_json).expect("decode host input");

            for received in [invocation, runtime, wire.into()] {
                if received.input.as_ref().is_some_and(|value| value["value"].is_number()) {
                    let schema = json!({"type":"object", "properties":{"value":{
                        "type":"integer", "const":9_007_199_254_740_993_u64
                    }}, "required":["value"]});
                    jsonschema::validator_for(&schema).unwrap().validate(received.input.as_ref().unwrap()).unwrap();
                }
                assert_eq!(
                    build_nested_tool_payload(
                        received.tool_kind,
                        &received.tool_name,
                        received.input
                    ),
                    expected
                );
            }
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_tool_search_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain(codex_tools::TOOL_SEARCH_TOOL_NAME),
            Some(json!({ "query": "example plugin", "limit": 8 })),
        )
        .expect("tool search payload should parse");

        assert_eq!(
            payload,
            ToolPayload::ToolSearch {
                arguments: SearchToolCallParams {
                    query: "example plugin".to_string(),
                    limit: Some(8),
                },
            }
        );
    }

    #[test]
    fn build_nested_tool_search_rejects_invalid_arguments() {
        let name = ToolName::plain(codex_tools::TOOL_SEARCH_TOOL_NAME);
        for input in [Some(json!(null)), Some(json!([])), Some(json!("query"))] {
            assert_eq!(
                build_nested_tool_payload(CodeModeToolKind::Function, &name, input),
                Err("tool `tool_search` expects a JSON object for arguments".to_string())
            );
        }
        for input in [
            None,
            Some(json!({"query": 3})),
            Some(json!({"query": "x", "limit": -1})),
        ] {
            assert!(
                build_nested_tool_payload(CodeModeToolKind::Function, &name, input)
                    .unwrap_err()
                    .starts_with("failed to parse tool `tool_search` arguments:")
            );
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_freeform_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Freeform,
            &ToolName::plain("example"),
            Some(json!("hello")),
        )
        .expect("freeform payload should preserve string input");

        match payload {
            ToolPayload::Custom { input } => {
                assert_eq!(input, "hello".to_string());
            }
            other => panic!("expected freeform payload, got {other:?}"),
        }
    }

    #[test]
    fn nested_failure_fingerprint_preserves_diagnostic_numbers() {
        let tool_name = ToolName::plain("example");
        assert_ne!(
            nested_failure_fingerprint(&tool_name, "request failed with status 403"),
            nested_failure_fingerprint(&tool_name, "request failed with status 404"),
        );
        assert_eq!(
            nested_failure_fingerprint(&tool_name, "request 17 failed"),
            nested_failure_fingerprint(&tool_name, "request  17\nfailed"),
        );
        assert_eq!(
            nested_failure_fingerprint(
                &tool_name,
                r#"tool failed: {"fingerprint":"owner.stable.failure"}"#,
            ),
            "owner.stable.failure"
        );
        assert_ne!(
            nested_failure_fingerprint(&tool_name, "request 17 failed after 2 attempts"),
            nested_failure_fingerprint(&tool_name, "request 91 failed after 8 attempts")
        );
        assert_ne!(
            nested_failure_fingerprint(&tool_name, "request 17 failed"),
            nested_failure_fingerprint(&ToolName::plain("other"), "request 17 failed")
        );
    }

    #[test]
    fn script_command_results_are_stable_without_losing_recovery_or_lifecycle() {
        let raw = serde_json::json!({
            "chunk_id": "random", "wall_time_seconds": 1.2,
            "original_token_count": 42, "original_token_count_is_approximate": true,
            "execution_state": "running", "process_exited": false,
            "session_id": 7, "session_capabilities": {"polling": true},
            "output": "progress", "output_reduced": true,
            "raw_output_artifact_id": "retained", "recovery_selector": {"kind": "lines", "start": 2, "end": 9}
        });
        let tool = ToolName::plain("exec_command");
        let projected = super::script_visible_nested_result(&tool, raw.clone(), "call-1", 0);
        let mut different = raw.clone();
        different["chunk_id"] = "other".into();
        different["wall_time_seconds"] = 9.4.into();
        different["original_token_count"] = 99.into();
        assert_eq!(projected, super::script_visible_nested_result(&tool, different, "call-1", 0));
        for key in ["session_id", "session_capabilities", "execution_state", "process_exited",
            "output", "output_reduced", "raw_output_artifact_id", "recovery_selector"] {
            assert_eq!(projected[key], raw[key]);
        }
        assert!(projected.get("wall_time_seconds").is_none());
        assert!(projected.get("original_token_count").is_none());
        assert_ne!(projected["chunk_id"],
            super::script_visible_nested_result(&tool, raw.clone(), "call-1", 1)["chunk_id"]);
        assert_eq!(super::script_visible_nested_result(&ToolName::plain("other"), raw.clone(), "call-1", 0), raw);
    }

    #[test]
    fn nested_patch_projection_keeps_partial_effects_and_retry() {
        let raw = serde_json::json!({"success":false,"changes_exact":false,
            "text":"failed after first file", "environment_id":"remote",
            "changes":[{"kind":"update","path":"first"}],
            "retry":{"patch_id":"retained","hunks":[2]}});
        assert_eq!(super::model_visible_nested_result(&ToolName::plain("apply_patch"), raw.clone()), raw);
        let custom = serde_json::json!({"success":true,"text":"custom diagnostic"});
        assert_eq!(super::model_visible_nested_result(&ToolName::plain("apply_patch"), custom.clone()), custom);
    }

    #[test]
    fn nested_execution_projection_preserves_lifecycle_and_recovery() {
        let raw = serde_json::json!({"exit_code": 7, "output": "assertion failed: left 3 right 7",
            "wall_time_seconds": 1.5, "process_exited": true, "chunk_id": "chunk",
            "output_reduced": false, "session_id": null});
        assert_eq!(
            super::model_visible_nested_result(
                &ToolName::plain("exec_command"),
                super::script_visible_nested_result(
                    &ToolName::plain("exec_command"), raw, "call-1", 0,
                ),
            ),
            serde_json::json!({"exit_code": 7, "output": "assertion failed: left 3 right 7",
                "process_exited": true, "current_chunk_display_complete": true, "session_id": null})
        );
        assert_eq!(
            super::model_visible_nested_result(
                &ToolName::plain("exec_command"),
                serde_json::json!({"session_id": 12, "output": "building", "output_reduced": true,
                "raw_output_artifact_id": "artifact"})
            ),
            serde_json::json!({"session_id": 12, "output": "building", "current_chunk_display_complete": false,
                "raw_output_artifact_id": "artifact", "retained_artifact_complete": false})
        );
        assert_eq!(
            super::model_visible_nested_result(
                &ToolName::plain("exec_command"),
                serde_json::json!({"exit_code": 2, "output": "src/run*: os error 123",
                "repair": "Command preflight advisory (rg_literal_glob_path): ..."})
            ),
            serde_json::json!({"exit_code": 2, "output": "src/run*: os error 123",
                "repair": "Command preflight advisory (rg_literal_glob_path): ..."})
        );
    }

    #[test]
    fn batched_recovery_envelopes_do_not_need_duplicate_receipts() {
        let selector = serde_json::json!({"kind":"lines","start":10,"end":20});
        let state = serde_json::json!({"raw_output_artifact_id":"retained", "recovery":{
            "tool":"read_tool_output", "arguments":{"artifact_id":"retained", "selectors":[selector]}
        }});
        let envelope = serde_json::json!({"raw_output_artifact_id":"retained", "recovery_selector":selector});
        for value in [
            envelope.clone(),
            serde_json::json!({"batch":[{"status":"fulfilled", "value":envelope}]}),
            serde_json::json!([{"recovery":state["recovery"]}]),
        ] {
            assert!(super::shows_command_recovery(&value.to_string(), &state));
        }
        for value in [
            serde_json::json!({"source":envelope.to_string()}),
            serde_json::json!({"artifact_id":"retained"}),
            serde_json::json!({"raw_output_artifact_id":"other", "recovery_selector":selector}),
            serde_json::json!({"raw_output_artifact_id":"retained", "recovery_selector":{"kind":"lines","start":1,"end":9}}),
            serde_json::json!({"recovery":null}),
        ] {
            assert!(!super::shows_command_recovery(&value.to_string(), &state));
        }
        assert!(!super::shows_command_recovery("{\"batch\":[", &state));
        assert!(!super::shows_command_recovery("{}", &serde_json::json!({})));
        let legacy = serde_json::json!({"raw_output_artifact_id":"retained"});
        assert!(super::shows_command_recovery("{\"batch\":[{\"artifact_id\":\"retained\"}]}", &legacy));
    }

    #[test]
    fn small_truncated_text_output_respects_the_complete_token_budget() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];

        let (truncated_items, omitted) =
            truncate_code_mode_result(items, Some(5), OutputOutcome::Success, usize::MAX, None);
        assert!(omitted);
        let [FunctionCallOutputContentItem::InputText { text }] = truncated_items.as_slice() else {
            panic!("expected text");
        };
        assert!(codex_utils_output_truncation::model_token_count(text) <= 5);
        assert!(text.contains('\u{2026}'));
        assert!(text.ends_with('9'));
    }

    #[tokio::test]
    async fn artifact_recovery_leaves_space_for_the_outer_exec_envelope() {
        use crate::tools::command_output_artifact::ToolOutputSelector;
        use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
        use crate::tools::command_output_artifact::create_canonical_output_artifact;
        use crate::tools::handlers::execute_recovery_transaction;
        use codex_tools::CanonicalToolResult;

        let home = tempfile::tempdir().expect("artifact home");
        // Both fixtures fit automatic direct recovery. The larger body exceeds the
        // nested aggregate budget, exercising the outer envelope reserve itself.
        for size in [9_000, 38_000] {
            let text = "x".repeat(size);
            let artifact = create_canonical_output_artifact(
                home.path(),
                "envelope-thread",
                &CanonicalToolResult::text(&text),
            )
            .await;
            let id = artifact.artifact_id().expect("canonical artifact admitted");
            let selectors = vec![ToolOutputSelector::Bytes {
                start: 0,
                end: size as u64,
            }];
            let (direct, _) = execute_recovery_transaction(
                home.path(),
                "envelope-thread",
                &id,
                selectors.clone(),
                false,
            )
            .await
            .expect("direct recovery");
            assert!(direct.complete);
            assert_eq!(direct.results[0].status, ToolOutputSelectorStatus::Ok);
            assert_eq!(direct.results[0].exact_bytes, Some(size as u64));
            assert_eq!(direct.results[0].text.as_deref(), Some(text.as_str()));
            let direct_tokens = codex_utils_string::approx_token_count(
                &serde_json::to_string(&direct).expect("serialize direct recovery"),
            );
            if size == 9_000 {
                assert!(
                    direct_tokens <= 9_000,
                    "fitting body must enter nested exact recovery"
                );
            } else {
                assert!(
                    direct_tokens > 9_128 && direct_tokens <= 10_000,
                    "actual serialized body must exceed nested recovery plus retry margin while fitting direct recovery"
                );
            }
            let (nested, _) =
                execute_recovery_transaction(home.path(), "envelope-thread", &id, selectors, true)
                    .await
                    .expect("code-mode recovery");
            if size == 9_000 {
                assert_eq!(nested.results[0].status, ToolOutputSelectorStatus::Ok);
                assert_eq!(nested.results[0].text.as_deref(), Some(text.as_str()));
            } else {
                assert_ne!(nested.results[0].status, ToolOutputSelectorStatus::Ok);
                assert!(
                    nested.results[0].text.is_none(),
                    "overflow must not clip exact text"
                );
                assert!(
                    !nested.results[0].child_selectors.is_empty()
                        || nested.results[0].continuation.is_some(),
                    "overflow must remain recoverable"
                );
            }
            let rendered = serde_json::to_string(&nested).expect("serialize nested recovery");
            let outer = format_runtime_response(
                RuntimeResponse::Result {
                    output_loss: None,
                    cell_id: CellId::new("recovery-envelope".to_string()),
                    content_items: vec![
                        codex_code_mode::FunctionCallOutputContentItem::InputText {
                            text: rendered.clone(),
                        },
                    ],
                    error_text: None,
                },
                Some(10_000),
                10_000,
                false,
                std::time::Instant::now(),
                Vec::new(),
                Vec::new(),
                None,
            );
            let projected =
                codex_protocol::models::function_call_output_content_items_to_text(&outer.body)
                    .expect("outer exec output");
            assert!(
                projected.contains(&rendered),
                "outer formatting must preserve the whole recovery JSON"
            );
            assert!(!projected.contains("[omitted lines "));
            assert!(
                !outer
                    .essential_inline
                    .contains_key(super::VISIBLE_OUTPUT_TRUNCATED_KEY)
            );
            assert!(
                codex_utils_string::approx_token_count(&projected) <= 10_000,
                "actual outer status plus recovery must fit the requested exec budget"
            );
        }
    }

    #[test]
    fn code_mode_truncation_preserves_full_canonical_recovery() {
        let sentinel = "CANONICAL_SENTINEL_AFTER_THE_MODEL_LIMIT";
        let output = format_runtime_response(
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: CellId::new("canonical-cell".to_string()),
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: format!("{}{}", "x".repeat(400), sentinel),
                }],
                error_text: None,
            },
            Some(5),
            5,
            false,
            std::time::Instant::now(),
            Vec::new(),
            Vec::new(),
            None,
        );

        let projected =
            codex_protocol::models::function_call_output_content_items_to_text(&output.body)
                .expect("projected code-mode text");
        assert!(projected.contains('…'));
        assert!(!projected.contains(sentinel));
        assert_eq!(
            output
                .essential_inline
                .get(super::VISIBLE_OUTPUT_TRUNCATED_KEY),
            Some(&serde_json::Value::Bool(true)),
            "admission must learn that the visible packet omitted output"
        );

        let canonical = output
            .canonical_result(&ToolPayload::Custom {
                input: "return a large result".to_string(),
            })
            .expect("canonical code-mode result");
        assert!(String::from_utf8_lossy(&canonical.bytes).contains(sentinel));
        let selector = &output.essential_inline["cell_output_recovery_selector"];
        assert_eq!(selector["kind"], "lines");
        let start = selector["start"].as_u64().unwrap() as usize;
        assert!(start > 1, "recovery skips the canonical status prefix");
        assert!(String::from_utf8_lossy(&canonical.bytes).lines().nth(start - 1).unwrap().contains(sentinel));
        assert!(
            output
                .projection_metadata()
                .expect("projection metadata")
                .spillable_text
                .iter()
                .any(|text| text.contains(sentinel)),
            "artifact admission must receive the complete provider output"
        );
    }

    #[test]
    fn code_mode_truncation_applies_hard_limit() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "x".repeat(400),
        }];

        let (truncated, omitted) =
            truncate_code_mode_result(items, Some(20), OutputOutcome::Success, 5, None);
        assert!(omitted);
        let [FunctionCallOutputContentItem::InputText { text }] = truncated.as_slice() else {
            panic!("expected one truncated text item");
        };
        assert!(codex_utils_string::approx_token_count(text) <= 5);
        assert!(!text.is_empty());
        assert!(!text.contains(&"x".repeat(100)));
    }

    #[test]
    fn code_mode_truncation_keeps_near_budget_packets_but_not_overflow() {
        let text = "output line\n".repeat(6_000);
        let tokens = codex_utils_output_truncation::model_token_count(&text);
        let budget = (tokens * 3).div_ceil(4);
        assert!(tokens <= budget + budget / 3);
        assert!(tokens > (budget - 1) + (budget - 1) / 3);
        let items = vec![FunctionCallOutputContentItem::InputText { text }];
        for outcome in [OutputOutcome::Success, OutputOutcome::Failure] {
            let diagnostic = (outcome == OutputOutcome::Failure).then_some(0);
            let (whole, omitted) = truncate_code_mode_result(
                items.clone(), Some(budget), outcome, usize::MAX, diagnostic,
            );
            assert_eq!(whole, items);
            assert!(!omitted);
            for (requested, hard) in [(budget - 1, usize::MAX), (budget, tokens - 1), (0, usize::MAX)] {
                let (cut, omitted) = truncate_code_mode_result(
                    items.clone(), Some(requested), outcome, hard, diagnostic,
                );
                assert!(omitted);
                assert!(codex_utils_output_truncation::model_token_count(&super::code_mode_text_content(&cut)) <= requested.min(hard));
            }
        }
        let text = "output line\n".repeat(20_000);
        assert!(codex_utils_output_truncation::model_token_count(&text) > codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
        let (cut, omitted) = truncate_code_mode_result(
            vec![FunctionCallOutputContentItem::InputText { text }],
            Some(usize::MAX), OutputOutcome::Success, usize::MAX, None,
        );
        assert!(omitted);
        assert!(codex_utils_output_truncation::model_token_count(&super::code_mode_text_content(&cut)) <= codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    }

    #[test]
    fn over_truncation_mixed_code_mode_failure_preserves_the_script_error() {
        let items = vec![
            FunctionCallOutputContentItem::InputText {
                text: format!(
                    "Script error:\nOLD_LOG {}",
                    "ordinary output ".repeat(1_000)
                ),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "opaque".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "Script error:\nROOT_CAUSE_SENTINEL".to_string(),
            },
        ];

        let (projected, omitted) =
            truncate_code_mode_result(items, Some(40), OutputOutcome::Failure, usize::MAX, Some(2));

        assert!(omitted, "the truncated log must be reported as omitted");
        assert!(projected.iter().any(|item| matches!(
            item,
            FunctionCallOutputContentItem::InputText { text }
                if text == "Script error:\nROOT_CAUSE_SENTINEL"
        )));
    }

    #[test]
    fn default_outer_budget_avoids_a_second_cut_and_respects_explicit_limits() {
        let text = "source line test\n".repeat(1_300);
        let original_tokens = codex_utils_output_truncation::model_token_count(&text);
        assert!(original_tokens > 4_000);
        assert!(original_tokens < codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
        let items = vec![FunctionCallOutputContentItem::InputText { text: text.clone() }];

        let (default_projection, omitted) =
            truncate_code_mode_result(items.clone(), None, OutputOutcome::Success, 10_000, None);
        assert!(!omitted);
        let default_text = super::code_mode_text_content(&default_projection);
        assert_eq!(default_text, text);
        let (small, omitted) = truncate_code_mode_result(
            items.clone(), Some(3_000), OutputOutcome::Success, 10_000, None);
        assert!(omitted);
        assert!(codex_utils_output_truncation::model_token_count(
            &super::code_mode_text_content(&small)) <= 3_000);
        let (projected, omitted) =
            truncate_code_mode_result(items, Some(10_000), OutputOutcome::Success, 10_000, None);

        let [
            FunctionCallOutputContentItem::InputText {
                text: projected_text,
            },
        ] = projected.as_slice()
        else {
            panic!("expected one projected text item");
        };
        assert_eq!(projected_text, &text);
        assert!(!omitted, "a fitting packet must not claim omitted output");

        let oversized = vec![FunctionCallOutputContentItem::InputText {
            text: "source line test\n".repeat(6_000),
        }];
        let (capped, omitted) =
            truncate_code_mode_result(oversized, None, OutputOutcome::Success, usize::MAX, None);
        assert!(omitted);
        let [FunctionCallOutputContentItem::InputText { text: capped_text }] = capped.as_slice()
        else {
            panic!("expected one capped text item");
        };
        assert!(capped_text.contains("[omitted lines "));
        let capped_tokens = codex_utils_output_truncation::model_token_count(capped_text);
        assert!(
            capped_tokens <= codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
            "the complete output must honor the hard limit; got {capped_tokens} tokens"
        );
    }

    #[tokio::test]
    async fn missing_process_host_is_reported_without_failing_service_creation() {
        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let error = service
            .execute(ExecuteRequest {
                state_path: None,
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new().into(),
                source: "text('unreachable')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            })
            .await
            .err()
            .expect("missing host should reject execution");

        assert!(error.contains("failed to spawn code-mode host"));
        service.shutdown().await.expect("shutdown unused service");
    }
}
