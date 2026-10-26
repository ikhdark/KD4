mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod repeated_bodies;
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
use futures::FutureExt;
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
    availability: Result<(), String>,
    unavailable_warning_emitted: AtomicBool,
    /// The model step that received the runtime-replacement notice.
    replacement_step: Mutex<std::sync::Weak<StepContext>>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    packet_admission: Mutex<CodeModePacketAdmission>,
    receipt_capacity: Arc<tokio::sync::Semaphore>,
    cell_parent_call_ids: Mutex<HashMap<String, String>>,
    shutting_down: AtomicBool,
    /// Input of the latest sampling request, which issued the running cells.
    visible_prompt: Mutex<Option<Arc<[codex_protocol::models::ResponseItem]>>>,
    /// Bodies last printed as a reference; the next print repeats them.
    elided_bodies: Mutex<std::collections::HashSet<[u8; 32]>>,
}

#[derive(Default)]
struct CodeModePacketAdmission {
    cells: HashMap<String, CodeModePacketMetrics>,
    terminal_cells: HashMap<String, CodeModePacketMetrics>,
    // Closed cells release execution state, not unresolved evidence. Keep only
    // exact recovery descriptors; the artifact owner retains the actual bytes.
    closed_recovery: HashMap<String, Vec<HashMap<String, DeliveryArtifactCoverage>>>,
    // By turn: the latest answer refused only for pending evidence recovery.
    retained_answers: HashMap<String, RetainedDeliveryAnswer>,
}

impl CodeModePacketAdmission {
    fn metrics_mut(&mut self, cell_id: &str) -> Option<&mut CodeModePacketMetrics> {
        if self.cells.contains_key(cell_id) {
            self.cells.get_mut(cell_id)
        } else {
            self.terminal_cells.get_mut(cell_id)
        }
    }
}

/// A finished answer whose delivery waited only on evidence recovery. Once the
/// evidence is recovered, a deliver cell that prints nothing delivers it
/// unchanged, so the model does not compose the same answer twice.
struct RetainedDeliveryAnswer {
    text: String,
    schema: Option<Arc<crate::session::turn_context::PreparedOutputSchema>>,
    input_activity: tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
}

#[derive(Default)]
struct CodeModePacketMetrics {
    receipt_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    terminal_parent_call_id: Option<String>,
    retention_tail: Option<futures::future::Shared<futures::future::BoxFuture<'static, ()>>>,
    turn_id: Option<String>,
    delivery_retired: bool,
    output_budget: Option<usize>,
    delivery_intent: Option<CodeModeDeliveryIntent>,
    delivery_blocked: bool,
    delivery_recovery: HashMap<String, DeliveryArtifactCoverage>,
    command_states: Vec<JsonValue>,
    next_nested_ordinal: usize,
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    nested_recovery: std::collections::BTreeMap<usize, JsonValue>,
    inline_recovery_bytes: usize,
    recovery_metadata_bytes: usize,
    pending_recovery_metadata: std::collections::BTreeMap<usize, usize>,
    /// `{call_id, tool_name}` of admitted calls that have not settled. Carried
    /// across packets like their reservations, so a terminal packet can name
    /// work the script stopped waiting for.
    pending_call_identities: std::collections::BTreeMap<usize, JsonValue>,
    /// Settled outcomes the script never observed: its wait was cancelled
    /// (wrapper deadline or cell end) and it saw only an error.
    unobserved_settlements: Vec<FunctionCallOutputContentItem>,
    omitted_unobserved_settlement_count: usize,
    omitted_nested_result_count: usize,
    first_required_terminal: Option<CodeModeNestedTerminal>,
    /// `read_tool_output` recoveries completed in this cell since its last
    /// delivered packet. That delivery is the only projection the recovered
    /// evidence crosses after its own bounded read.
    tool_output_recovery_count: u32,
}

struct CodeModeDeliveryIntent {
    turn_id: String,
    input_activity: tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
    schema: Option<Arc<crate::session::turn_context::PreparedOutputSchema>>,
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
    fn missing_ranges(&self, limit: usize) -> (Vec<(u64, u64)>, bool) {
        let mut required = if self.required_ranges.is_empty() {
            vec![(0, self.bytes)]
        } else {
            self.required_ranges.clone()
        };
        required.sort_unstable();
        let mut missing = Vec::new();
        let mut required_end = 0;
        for (start, end) in required {
            let mut cursor = start.max(required_end);
            required_end = required_end.max(end);
            for &(covered_start, covered_end) in &self.ranges {
                if cursor >= end {
                    break;
                }
                if covered_end <= cursor {
                    continue;
                }
                if covered_start > cursor {
                    missing.push((cursor, covered_start.min(end)));
                    if missing.len() > limit {
                        missing.truncate(limit);
                        return (missing, true);
                    }
                }
                cursor = cursor.max(covered_end);
            }
            if cursor < end {
                missing.push((cursor, end));
                if missing.len() > limit {
                    missing.truncate(limit);
                    return (missing, true);
                }
            }
        }
        (missing, false)
    }

    fn recovered(&self) -> bool {
        if self.required_ranges.is_empty() {
            self.bytes == 0
                || self
                    .ranges
                    .first()
                    .is_some_and(|range| *range == (0, self.bytes))
        } else {
            self.required_ranges.iter().all(|(start, end)| {
                start == end
                    || self
                        .ranges
                        .iter()
                        .any(|range| range.0 <= *start && *end <= range.1)
            })
        }
    }

    fn observe(&mut self, evidence: &JsonValue) {
        if evidence["identity"]["sha256"].as_str() != Some(self.sha256.as_str()) {
            return;
        }
        // Parsed values preserve semantic data, not exact source coverage:
        // duplicate keys, escaping and whitespace may have been discarded.
        let ranges = evidence["identity"]["ranges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|range| Some((range[0].as_u64()?, range[1].as_u64()?)))
            .collect::<Vec<_>>();
        self.cover(ranges);
    }

    /// Records exact byte ranges already delivered from this artifact.
    fn cover(&mut self, ranges: impl IntoIterator<Item = (u64, u64)>) {
        let bytes = self.bytes;
        self.ranges.extend(
            ranges
                .into_iter()
                .filter(|(start, end)| start <= end && *end <= bytes),
        );
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        self.ranges = merged;
    }
}

fn delivery_recovery_refusal<'a>(
    obligations: impl Iterator<Item = (&'a String, &'a DeliveryArtifactCoverage)>,
) -> JsonValue {
    const MAX_RECOVERY_ITEMS: usize = 8;
    const MAX_RECOVERY_RANGES: usize = 8;
    let mut pending = obligations
        .filter(|(_, coverage)| !coverage.recovered())
        .collect::<Vec<_>>();
    pending.sort_by(|(left_id, left), (right_id, right)| {
        (left_id, &left.sha256, &left.required_ranges).cmp(&(
            right_id,
            &right.sha256,
            &right.required_ranges,
        ))
    });
    let omitted_recovery_count = pending.len().saturating_sub(MAX_RECOVERY_ITEMS);
    let recovery = pending
        .into_iter()
        .take(MAX_RECOVERY_ITEMS)
        .map(|(id, coverage)| {
            let (ranges, more_ranges) = coverage.missing_ranges(MAX_RECOVERY_RANGES);
            let selectors = ranges
                .into_iter()
                .map(|(start, end)| serde_json::json!({"kind":"bytes", "start":start, "end":end}))
                .collect::<Vec<_>>();
            serde_json::json!({
                "artifact_id": id,
                "canonical_sha256": coverage.sha256,
                "canonical_bytes": coverage.bytes,
                "tool": "read_tool_output",
                "arguments": {"artifact_id": id, "selectors": selectors},
                "more_missing_ranges": more_ranges,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "category": "evidence_recovery_pending",
        "recovery": recovery,
        "omitted_recovery_count": omitted_recovery_count,
    })
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
    post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
    nested_results: Vec<CodeModeNestedResultEvidence>,
    nested_recovery: std::collections::BTreeMap<usize, JsonValue>,
    omitted_nested_result_count: usize,
    first_required_terminal: Option<CodeModeNestedTerminal>,
    tool_output_recovery_count: u32,
    unsettled_nested_calls: Vec<JsonValue>,
    unobserved_settlements: Vec<FunctionCallOutputContentItem>,
    omitted_unobserved_settlement_count: usize,
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

/// Retain a *failed* nested result within the same byte budget, keeping both
/// ends of the message instead of only its head.
///
/// The harness composes a rejection as cause-then-remedy: "Validation busy:
/// holder command ... Resume the existing operation rather than relaunching",
/// "unable to read ...\nPossible existing paths (not verified replacements):
/// ...", "Reason: PowerShell reported invalid script syntax ... Use: Correct
/// the script syntax". A head-only bound keeps the cause and drops the remedy,
/// so the model is told what went wrong with no stated way forward.
///
/// Saved sessions show this is the common case, not an edge case: of 96
/// recorded failed nested entries, 70 were truncated with no remedy text left
/// and `recovery_available: false`.
///
/// The budget is unchanged. Successful results keep the existing head-only
/// behaviour: their tail is ordinary payload, not guidance.
fn bounded_failed_nested_json(value: &JsonValue) -> (String, bool, usize) {
    let (head, truncated, total) = bounded_serialized_json(value);
    if !truncated {
        return (head, truncated, total);
    }
    let Ok(full) = serde_json::to_string(value) else {
        return (head, truncated, total);
    };
    bounded_failed_nested_text(&full)
}

fn bounded_failed_nested_text(full: &str) -> (String, bool, usize) {
    if full.len() <= MAX_RETAINED_NESTED_RESULT_BYTES {
        return (full.to_string(), false, full.len());
    }
    // Spend the budget on a head and a tail with an explicit elision between
    // them. The result is deliberately not valid JSON: it is a diagnostic
    // projection, and the recovery directory remains the structured route.
    let budget = MAX_RETAINED_NESTED_RESULT_BYTES.saturating_sub(ELISION.len());
    let head_budget = budget / 2;
    let tail_budget = budget.saturating_sub(head_budget);
    let head_end = floor_char_boundary(full, head_budget);
    let tail_start = ceil_char_boundary(full, full.len().saturating_sub(tail_budget));
    if tail_start <= head_end {
        return (
            full[..floor_char_boundary(full, MAX_RETAINED_NESTED_RESULT_BYTES)].to_string(),
            true,
            full.len(),
        );
    }
    (
        format!("{}{ELISION}{}", &full[..head_end], &full[tail_start..]),
        true,
        full.len(),
    )
}

/// Marks where the middle of a bounded failure message was removed.
const ELISION: &str = "… [middle omitted; see nested_result_recovery_directory] …";

/// `str::floor_char_boundary` is unstable, so bound the index by hand rather
/// than risk splitting a multi-byte character.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

const MAX_RETAINED_NESTED_RESULTS: usize = 2;
const MAX_RETAINED_NESTED_RESULT_BYTES: usize = 1_024;
// Bounded receipts for nested work the script stopped waiting for.
const MAX_UNOBSERVED_SETTLEMENTS: usize = 8;
const MAX_LISTED_UNSETTLED_NESTED_CALLS: usize = 16;
// Diagnostic previews are not a storage threshold. Retain ordinary native
// results in the packet; spill only when its aggregate recovery budget is full.
const MAX_INLINE_PACKET_RECOVERY_BYTES: usize = 1024 * 1024;
// Full results already spill through the artifact owner. Reserve space for
// their recovery descriptors too, before dispatch can create another effect.
const MAX_PACKET_RECOVERY_METADATA_BYTES: usize = 8 * 1024 * 1024;
const PACKET_RECOVERY_METADATA_BASE_BYTES: usize = 8 * 1024;
const MAX_FAILED_CELL_ERROR_BYTES: usize = 4_096;
const FAILED_CELL_ERROR_TRUNCATION_MARKER: &str = "\n… [truncated]";
const APPLY_PATCH_ENVELOPE_MARKER: &str = "*** Begin Patch";
const RUNTIME_REPLACED_EARLIER_IN_STEP: &str = "This cell did not run: an earlier `exec` call in this response reported that the Code Mode runtime was replaced and its stored values were lost. Read that result, then run the cell again.";

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new());
        let availability = session_provider.availability();
        Self {
            session: OnceCell::new(),
            recovery_path: None,
            session_provider,
            availability,
            unavailable_warning_emitted: AtomicBool::new(false),
            replacement_step: Mutex::new(std::sync::Weak::new()),
            dispatch_broker,
            packet_admission: Mutex::new(CodeModePacketAdmission::default()),
            receipt_capacity: Arc::new(tokio::sync::Semaphore::new(256)),
            cell_parent_call_ids: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            visible_prompt: Mutex::new(None),
            elided_bodies: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Records the input of a sampling request before it is sent. Cells it
    /// issues do not print source bodies again that it holds verbatim.
    pub(crate) fn note_visible_prompt(&self, items: Arc<[codex_protocol::models::ResponseItem]>) {
        *self
            .visible_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(items);
    }

    pub(super) fn elide_bodies_already_in_context(&self, output: &mut FunctionToolOutput) {
        let Some(prompt) = self
            .visible_prompt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let mut elided = self
            .elided_bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        repeated_bodies::elide_bodies_already_in_context(&mut output.body, &prompt, &mut elided);
    }

    pub(crate) fn with_recovery_path(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.recovery_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    /// Whether cells can run at all. Checked once: an unavailable host makes
    /// ordinary Code Mode turns use direct tools instead.
    pub(crate) fn is_available(&self) -> bool {
        self.availability.is_ok()
    }

    /// Returns the unavailability warning the first time it is requested.
    pub(crate) fn take_unavailable_warning(&self, tool_mode: ToolMode) -> Option<String> {
        let error = self.availability.as_ref().err()?;
        let behavior = match tool_mode {
            ToolMode::Direct => "Falling back to direct tools",
            ToolMode::CodeMode | ToolMode::CodeModeOnly => "Code mode will fail closed",
        };
        (!self.unavailable_warning_emitted.swap(true, Ordering::Relaxed)).then(|| {
            format!(
                "Code Mode is unavailable because {error}. {behavior}; enable `features.code_mode_host` and install `codex-code-mode-host`."
            )
        })
    }


    /// Runs a cell the model requested in `step_context`. Once one cell of a
    /// step receives the runtime-replacement notice, the step's later cells
    /// were written without seeing it, so they do not run either.
    pub(crate) async fn execute_for_step(
        &self,
        request: codex_code_mode::ExecuteRequest,
        step_context: &Arc<StepContext>,
    ) -> Result<codex_code_mode::StartedCell, String> {
        self.seal_completed_cells();
        // Terminal custody cannot grow without bound when callers abandon
        // yielded cells. Refuse only new execution; never evict known effects.
        let permit = Arc::clone(&self.receipt_capacity).try_acquire_owned().map_err(|_| {
            "Code Mode has 256 unobserved cells; use wait on returned cell IDs to collect their receipts before starting new work. No new cell started.".to_string()
        })?;
        let replaced_earlier_in_step = self
            .replacement_step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
            .is_some_and(|step| Arc::ptr_eq(&step, step_context));
        if replaced_earlier_in_step {
            return Err(RUNTIME_REPLACED_EARLIER_IN_STEP.to_string());
        }
        // Worker construction does not make the runtime ready. Measure the
        // actual shared initialization wait, including cancelled waiters.
        let readiness = step_context
            .turn
            .turn_timing_state
            .begin_local_phase(crate::turn_timing::TurnLocalPhase::ExecutorReadinessWait);
        let session = self.session().await;
        drop(readiness);
        let result = match session {
            Ok(session) => session.execute(request).await,
            Err(error) => Err(error),
        };
        if matches!(&result, Err(error) if error == codex_code_mode::RUNTIME_REPLACED_NOTICE) {
            *self
                .replacement_step
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(step_context);
        }
        if let Ok(started) = &result {
            self.packet_admission.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                .cells.entry(started.cell_id.to_string()).or_default().receipt_permit = Some(permit);
        }
        result
    }

    pub(crate) async fn wait(
        &self,
        mut request: codex_code_mode::WaitRequest,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.seal_completed_cells();
        self.restore_terminal_packet(request.cell_id.as_str());
        request.recovery =
            self.recovery_path
                .clone()
                .map(|path| codex_code_mode::ReceiptRecovery {
                    path,
                    terminal_only: false,
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


    pub(crate) fn bind_cell_environments(&self, cell_id: &CellId, step: &StepContext) {
        self.dispatch_broker
            .bind_cell_environments(cell_id, &step.environments);
        self.dispatch_broker
            .bind_cell_user_intent(cell_id, step.user_intent_epoch);
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

    fn seal_completed_cells(&self) {
        for cell_id in self.dispatch_broker.take_closed_cells() {
            let parent = self
                .cell_parent_call_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(cell_id.as_str());
            let mut admission = self
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mut packet) = admission.cells.remove(cell_id.as_str()) {
                packet.terminal_parent_call_id = parent;
                admission.terminal_cells.insert(cell_id.to_string(), packet);
            }
        }
    }

    // This transfers observation custody only. The broker/runtime remain closed
    // and cannot dispatch new actions on behalf of a terminal receipt.
    fn restore_terminal_packet(&self, cell_id: &str) {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(packet) = admission.terminal_cells.remove(cell_id) {
            admission.cells.insert(cell_id.to_string(), packet);
        }
    }

    pub(crate) fn record_output_budget(&self, cell_id: &CellId, budget: Option<usize>) {
        self.restore_terminal_packet(cell_id.as_str());
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
            && budget.is_some()
        {
            metrics.output_budget = budget;
        }
    }

    /// Attributes a completed in-cell `read_tool_output` recovery to the
    /// cell's next delivered packet. A closed packet is not recreated.
    pub(crate) fn record_packet_tool_output_recovery(&self, cell_id: &str) {
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id)
        {
            metrics.tool_output_recovery_count =
                metrics.tool_output_recovery_count.saturating_add(1);
        }
    }

    /// Names an admitted nested call until it settles, so a terminal packet
    /// can report work the script stopped waiting for. A closed packet is not
    /// recreated.
    fn record_packet_call_identity(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        call_id: &str,
        tool_name: &str,
    ) {
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
        {
            metrics.pending_call_identities.insert(
                ordinal,
                serde_json::json!({"call_id": call_id, "tool_name": tool_name}),
            );
        }
    }

    /// Queues a settled outcome the script could not observe for the cell's
    /// next packet. Bounded, so a cancellation burst cannot crowd out the
    /// script's own output. A closed packet is not recreated.
    fn record_unobserved_settlement(&self, cell_id: &CellId, settlement: JsonValue) {
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .metrics_mut(cell_id.as_str())
        {
            if metrics.unobserved_settlements.len() < MAX_UNOBSERVED_SETTLEMENTS {
                metrics
                    .unobserved_settlements
                    .push(FunctionCallOutputContentItem::InputText {
                        text: settlement.to_string(),
                    });
            } else {
                metrics.omitted_unobserved_settlement_count =
                    metrics.omitted_unobserved_settlement_count.saturating_add(1);
            }
        }
    }

    pub(super) fn record_cell_turn(&self, cell_id: &CellId, turn_id: &str) {
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
        {
            metrics.turn_id = Some(turn_id.to_string());
        }
    }

    pub(super) fn bind_cell_delivery_turn(&self, cell_id: &CellId, turn: &TurnContext) {
        // Admission and terminal sealing already share this owner. A late
        // execute registration cannot resurrect a retired turn's obligations.
        if !turn.tool_call_acceptance.try_accept(|| {
            self.record_cell_turn(cell_id, &turn.sub_id);
            true
        }) && let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
        {
            metrics.delivery_retired = true;
        }
    }

    pub(crate) fn retire_delivery_turn(&self, turn_id: &str) {
        self.seal_completed_cells();
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admission.closed_recovery.remove(turn_id);
        admission.retained_answers.remove(turn_id);
        for metrics in admission
            .cells
            .values_mut()
            .filter(|metrics| metrics.turn_id.as_deref() == Some(turn_id))
        {
            metrics.delivery_retired = true;
            metrics.delivery_intent = None;
            metrics.delivery_recovery.clear();
        }
        for metrics in admission.terminal_cells.values_mut().filter(|metrics| metrics.turn_id.as_deref() == Some(turn_id)) {
            metrics.delivery_retired = true;
            metrics.delivery_intent = None;
            metrics.delivery_recovery.clear();
        }
    }

    pub(crate) fn output_budget(&self, cell_id: &str) -> Option<usize> {
        let admission = self.packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admission.cells.get(cell_id).or_else(|| admission.terminal_cells.get(cell_id)).and_then(|metrics| metrics.output_budget)
    }

    pub(super) fn record_delivery_intent(
        &self,
        cell_id: &CellId,
        turn: &TurnContext,
        input_activity: tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
    ) {
        if let Some(metrics) = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
        {
            if metrics.delivery_retired {
                return;
            }
            metrics.delivery_intent = Some(CodeModeDeliveryIntent {
                turn_id: turn.sub_id.clone(),
                input_activity,
                schema: turn.final_output_json_schema.clone(),
                limit: metrics
                    .output_budget
                    .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL),
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
        self.restore_terminal_packet(cell_id.as_str());
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let CodeModePacketAdmission {
            cells,
            closed_recovery,
            retained_answers,
            ..
        } = &mut *admission;
        let prior_recovery_pending = closed_recovery
            .get(&turn.sub_id)
            .is_some_and(|cells| !cells.is_empty());
        let Some(metrics) = cells.get_mut(cell_id.as_str()) else {
            return Ok(None);
        };
        if let RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::ExplicitYield { content_items, .. } = response
        {
            // A partial visible answer cannot safely be delivered again, or
            // reconstructed from an incomplete tail. Empty yields retain intent.
            let requested = metrics.delivery_intent.is_some();
            if !content_items.is_empty() {
                metrics.delivery_intent = None;
            }
            return if requested {
                Err(serde_json::json!({"category":if content_items.is_empty() {
                "cell_running" } else { "partial_output" }, "cell_id":cell_id.as_str()}))
            } else {
                Ok(None)
            };
        }
        let Some(intent) = metrics.delivery_intent.take() else {
            return Ok(None);
        };
        let refusal = if intent.turn_id != turn.sub_id {
            Some("turn_changed")
        } else if intent.schema != turn.final_output_json_schema {
            Some("schema_changed")
        } else if intent.input_activity.has_changed().unwrap_or(true) {
            Some("input_changed")
        } else if metrics.delivery_blocked {
            Some("nested_work_failed_or_incomplete")
        } else if !metrics.pending_recovery_metadata.is_empty() {
            // An admitted call whose script-side wait ended (deadline or
            // cancellation) may still be settling. The script finishing is
            // not that call's outcome, so its effects are still unknown.
            Some("nested_work_unsettled")
        } else if prior_recovery_pending
            || metrics
                .delivery_recovery
                .values()
                .any(|coverage| !coverage.recovered())
        {
            Some("evidence_recovery_pending")
        } else if metrics.first_required_terminal.is_some() {
            Some("required_tool_failed")
        } else {
            None
        };
        if let Some(category) = refusal {
            if category == "evidence_recovery_pending" {
                // Explain the existing gate without weakening it. Include prior
                // closed cells so a later delivery can recover the actual gap.
                let mut refusal = delivery_recovery_refusal(
                    metrics.delivery_recovery.iter().chain(
                        closed_recovery
                            .get(&turn.sub_id)
                            .into_iter()
                            .flatten()
                            .flat_map(|cell| cell.iter()),
                    ),
                );
                let limit = intent
                    .limit
                    .min(metrics.output_budget.unwrap_or(intent.limit));
                if let Ok(text) = execute_handler::schema_validated_delivery(
                    response,
                    limit,
                    intent.schema.as_deref(),
                ) {
                    retained_answers.clear();
                    retained_answers.insert(
                        turn.sub_id.clone(),
                        RetainedDeliveryAnswer {
                            text,
                            schema: intent.schema.clone(),
                            input_activity: intent.input_activity,
                        },
                    );
                }
                if retained_answers.contains_key(&turn.sub_id) {
                    refusal["retained_answer"] = "kept; once recovered, a deliver cell that prints nothing delivers it unchanged. Print a new answer instead if the recovered evidence changes it.".into();
                }
                return Err(refusal);
            }
            return Err(serde_json::json!({"category":category}));
        }
        if let Some(state) = metrics.command_states.iter().find(|state| {
            // Absence of a running flag is not terminal proof. The command
            // owner must settle execution and deferred work affirmatively.
            state.get("execution_state").and_then(JsonValue::as_str) != Some("exited")
                || state.get("session_id").is_some_and(|id| !id.is_null())
                || state.get("process_exited").and_then(JsonValue::as_bool) != Some(true)
                || !(state.get("exit_code").and_then(JsonValue::as_i64) == Some(0)
                    || (state["search_no_match"] == true && state["exit_code"] == 1))
                || state.get("error").is_some_and(|error| !error.is_null())
                || state
                    .get("pending_deferred_completions")
                    .is_some_and(|pending| !pending.as_array().is_some_and(Vec::is_empty))
        }) {
            let category = if state
                .get("pending_deferred_completions")
                .is_some_and(|pending| !pending.as_array().is_some_and(Vec::is_empty))
            {
                "deferred_work_pending"
            } else if state["execution_state"] == "running" || state["session_id"].is_number() {
                "command_running_or_draining"
            } else {
                "command_failed_or_unverified"
            };
            return Err(
                serde_json::json!({"category":category, "session_id":state["session_id"],
                "execution_state":state["execution_state"], "exit_code":state["exit_code"]}),
            );
        }
        let limit = intent
            .limit
            .min(metrics.output_budget.unwrap_or(intent.limit));
        let delivery =
            execute_handler::schema_validated_delivery(response, limit, intent.schema.as_deref());
        let Some(retained) = retained_answers.remove(&turn.sub_id).filter(|_| {
            delivery
                .as_ref()
                .is_err_and(|refusal| refusal["category"] == "empty_output")
        }) else {
            return delivery.map(Some);
        };
        // The retry printed nothing: deliver the answer kept at the refusal,
        // unless new input or a new output schema may have outdated it.
        if retained.schema != intent.schema || retained.input_activity.has_changed().unwrap_or(true)
        {
            return Err(serde_json::json!({"category":"retained_answer_stale",
                "reason":"input or the output schema changed after the refusal; print the answer"}));
        }
        if codex_utils_output_truncation::model_token_count(&retained.text) > limit {
            return Err(serde_json::json!({"category":"output_budget", "limit":limit}));
        }
        Ok(Some(retained.text))
    }

    pub(crate) fn cell_parent_call_id(&self, cell_id: &CellId) -> Option<String> {
        let active = self
            .cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id.as_str())
            .cloned();
        active.or_else(|| {
            let admission = self
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            admission
                .cells
                .get(cell_id.as_str())
                .or_else(|| admission.terminal_cells.get(cell_id.as_str()))
                .and_then(|metrics| metrics.terminal_parent_call_id.clone())
        })
    }

    fn record_delivery_evidence(
        &self,
        cell_id: &CellId,
        artifact_required: bool,
        artifact: Option<(String, String, u64)>,
        signal: Option<&JsonValue>,
        value: &JsonValue,
    ) -> Option<JsonValue> {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission.metrics_mut(cell_id.as_str())?;
        let mut recovery = None;
        if artifact_required {
            if let Some((id, sha256, bytes)) = artifact {
                let text = value.as_str().or_else(|| value["output"].as_str());
                let parsed = text.and_then(|text| serde_json::from_str::<JsonValue>(text).ok());
                let exact = text
                    .is_some_and(|text| hex::encode(Sha256::digest(text.as_bytes())) == sha256)
                    || serde_json::to_vec(parsed.as_ref().unwrap_or(value))
                        .is_ok_and(|bytes| hex::encode(Sha256::digest(bytes)) == sha256);
                let coverage = metrics
                    .delivery_recovery
                    .entry(id.clone())
                    .or_insert_with(|| DeliveryArtifactCoverage {
                        sha256,
                        bytes,
                        ranges: Vec::new(),
                        required_ranges: Vec::new(),
                    });
                // An originating canonical-output obligation covers the whole
                // artifact, even if earlier calls requested only a subset.
                coverage.required_ranges.clear();
                if exact {
                    coverage.ranges = vec![(0, coverage.bytes)];
                }
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
                value["artifact_id"].as_str(),
                value["canonical_sha256"].as_str(),
                value["canonical_bytes"].as_u64(),
                value["results"].as_array(),
            )
        {
            let range = |result: &JsonValue| {
                let start = result["canonical_range"]["start"].as_u64()?;
                let end = result["canonical_range"]["end"].as_u64()?;
                (start <= end && end <= bytes).then_some((start, end))
            };
            // A selector error makes the whole read incomplete without hiding
            // the selections that were delivered. Only unfinished results owe
            // recovery; finished ones already delivered their exact ranges.
            let mut required_ranges = Vec::new();
            let mut delivered_ranges = Vec::new();
            let mut unauthenticated = false;
            for result in results {
                match (result["complete"] == true, range(result)) {
                    (true, delivered) => delivered_ranges.extend(delivered),
                    (false, Some(required)) => required_ranges.push(required),
                    (false, None) => unauthenticated = true,
                }
            }
            if unauthenticated {
                // A cursor without authenticated ranges is not proof of exact
                // coverage. Do not make an incomplete search deliverable.
                metrics.delivery_blocked = true;
            } else if !required_ranges.is_empty() {
                let coverage = match metrics.delivery_recovery.entry(id.to_string()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(DeliveryArtifactCoverage {
                            sha256: sha256.to_string(),
                            bytes,
                            ranges: Vec::new(),
                            required_ranges,
                        })
                    }
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        let coverage = entry.into_mut();
                        if !coverage.required_ranges.is_empty() {
                            coverage.required_ranges.extend(required_ranges);
                        }
                        coverage
                    }
                };
                if coverage.sha256 == sha256 {
                    coverage.cover(delivered_ranges);
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
        metrics
            .delivery_recovery
            .retain(|_, coverage| !coverage.recovered());
        if let Some(evidence) = signal.and_then(|signal| signal.get("semantic_evidence"))
            && evidence["source"] == "artifact"
            && let Some(id) = evidence["scope"].as_str()
        {
            // Authenticated recovery is turn evidence, not a property of the
            // cell that happened to perform the read. Reconcile live siblings
            // under the same owner/lock as closed obligations.
            let turn_id = admission
                .cells
                .get(cell_id.as_str())
                .and_then(|cell| cell.turn_id.clone());
            if let Some(turn_id) = turn_id {
                for cell in admission
                    .cells
                    .values_mut()
                    .filter(|cell| cell.turn_id.as_ref() == Some(&turn_id))
                {
                    if let Some(required) = cell.delivery_recovery.get_mut(id) {
                        required.observe(evidence);
                        if required.recovered() {
                            cell.delivery_recovery.remove(id);
                        }
                    }
                }
                for cell in admission
                    .terminal_cells
                    .values_mut()
                    .filter(|cell| cell.turn_id.as_ref() == Some(&turn_id))
                {
                    if let Some(required) = cell.delivery_recovery.get_mut(id) {
                        required.observe(evidence);
                        if required.recovered() {
                            cell.delivery_recovery.remove(id);
                        }
                    }
                }
            }
            admission.closed_recovery.retain(|_, cells| {
                cells.retain_mut(|coverage| {
                    if let Some(required) = coverage.get_mut(id) {
                        required.observe(evidence);
                        if required.recovered() {
                            coverage.remove(id);
                        }
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
        // Only registration creates packet state. Accepted child operations
        // keep their own cleanup owner, but cannot recreate a closed packet.
        {
            let mut admission = self
                .packet_admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(metrics) = admission.cells.remove(cell_id.as_str()).or_else(|| admission.terminal_cells.remove(cell_id.as_str()))
                && !metrics.delivery_retired
                && !metrics.delivery_recovery.is_empty()
                && let Some(turn_id) = metrics.turn_id
            {
                admission
                    .closed_recovery
                    .entry(turn_id)
                    .or_default()
                    .push(metrics.delivery_recovery);
            }
        }
        self.cell_parent_call_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(cell_id.as_str());
    }


    fn admit_packet_call(
        &self,
        cell_id: &CellId,
        identity_bytes: usize,
    ) -> Result<usize, &'static str> {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = admission
            .cells
            .get_mut(cell_id.as_str())
            .ok_or("code mode cell is closed")?;
        // Worst-case JSON escaping of identities plus the bounded preview or
        // artifact descriptor. Reservations include failed calls and pending
        // retention; repeated small results cannot grow an uncharged index.
        let bytes = identity_bytes
            .saturating_mul(6)
            .saturating_add(PACKET_RECOVERY_METADATA_BASE_BYTES);
        let total = metrics.recovery_metadata_bytes.saturating_add(bytes);
        if total > MAX_PACKET_RECOVERY_METADATA_BYTES {
            metrics.delivery_blocked = true;
            return Err(
                "code mode recovery metadata capacity reached; this nested tool did not run. Await started work and use yield_control() to deliver retained receipts before continuing only unexecuted work; do not replay the cell",
            );
        }
        let ordinal = metrics.next_nested_ordinal;
        metrics.next_nested_ordinal = metrics.next_nested_ordinal.saturating_add(1);
        metrics.recovery_metadata_bytes = total;
        metrics.pending_recovery_metadata.insert(ordinal, bytes);
        Ok(ordinal)
    }

    fn try_record_inline_packet_recovery(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        result_bytes: usize,
        recovery: impl FnOnce() -> JsonValue,
    ) -> bool {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metrics) = admission.metrics_mut(cell_id.as_str()) else {
            return false;
        };
        if result_bytes
            > MAX_INLINE_PACKET_RECOVERY_BYTES.saturating_sub(metrics.inline_recovery_bytes)
        {
            return false;
        }
        let recovery = recovery();
        // Charge the complete envelope, not just its result. The fast check
        // above avoids cloning an already-oversized result into the envelope.
        let mut counter = BoundedJsonWriter::new(0);
        if serde_json::to_writer(&mut counter, &recovery).is_err()
            || counter.total_bytes
                > MAX_INLINE_PACKET_RECOVERY_BYTES.saturating_sub(metrics.inline_recovery_bytes)
        {
            return false;
        }
        metrics.inline_recovery_bytes += counter.total_bytes;
        metrics.nested_recovery.insert(ordinal, recovery);
        true
    }

    fn queue_packet_retention(
        &self,
        session: &Session,
        cell_id: &CellId,
        retention: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Option<futures::future::Shared<futures::future::BoxFuture<'static, ()>>> {
        let (done, receiver) = tokio::sync::oneshot::channel();
        let tail = async move {
            let _ = receiver.await;
        }
        .boxed()
        .shared();
        let previous = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get_mut(cell_id.as_str())
            .and_then(|metrics| metrics.retention_tail.replace(tail));
        // Session shutdown owns accepted writes, even if a caller is cancelled.
        // A closed packet is never recreated by late persistence completion.
        let backpressure = previous.clone();
        session.terminal_tasks.spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            retention.await;
            let _ = done.send(());
        });
        // Do not let a sequential producer accumulate full results behind slow
        // storage. Wait for its predecessor, not this write: the next tool can
        // still overlap persistence. Concurrent producers remain bounded by the
        // runtime's outstanding-callback limit. Enqueue before exposing the wait
        // so cancelling the observer never discards an accepted result.
        backpressure
    }

    async fn flush_packet_retention(&self, cell_id: &CellId) {
        // Snapshot without removing the tail: cancellation of a wait must not
        // break ordering, and a live cell may enqueue more work during a yield.
        let tail = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cells
            .get(cell_id.as_str())
            .and_then(|metrics| metrics.retention_tail.clone());
        if let Some(tail) = tail {
            tail.await;
        }
    }

    async fn observe_packet_retention(
        &self,
        cell_id: &CellId,
        terminating: bool,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> bool {
        let flush = self.flush_packet_retention(cell_id);
        tokio::pin!(flush);
        tokio::select! {
            biased;
            _ = &mut flush => true,
            _ = cancellation.cancelled() => false,
            _ = async {
                if terminating { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }
                else { std::future::pending::<()>().await; }
            } => false,
        }
    }

    fn record_packet_recovery(&self, cell_id: &CellId, ordinal: usize, recovery: JsonValue) -> bool {
        let mut admission = self.packet_admission.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = if admission.cells.contains_key(cell_id.as_str()) {
            admission.cells.get_mut(cell_id.as_str())
        } else { admission.terminal_cells.get_mut(cell_id.as_str()) };
        if let Some(metrics) = metrics
        {
            metrics.nested_recovery.insert(ordinal, recovery);
            return true;
        }
        false
    }

    fn complete_packet_call(
        &self,
        cell_id: &CellId,
        ordinal: usize,
        post_tool_use_feedback: Vec<FunctionCallOutputContentItem>,
        nested_result: Option<CodeModeNestedResultEvidence>,
        required_terminal: Option<(RequiredToolTerminalCause, String)>,
    ) {
        let mut admission = self
            .packet_admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metrics) = admission.metrics_mut(cell_id.as_str()) else {
            return;
        };
        metrics.pending_recovery_metadata.remove(&ordinal);
        metrics.pending_call_identities.remove(&ordinal);
        metrics.delivery_blocked |= required_terminal.is_some()
            || nested_result.as_ref().is_some_and(|result| result.failed)
            || !post_tool_use_feedback.is_empty();
        metrics
            .post_tool_use_feedback
            .extend(post_tool_use_feedback);
        if let Some(nested_result) = nested_result {
            metrics.nested_recovery.entry(ordinal).or_insert_with(|| {
                serde_json::json!({
                    "call_id": nested_result.call_id, "tool_name": nested_result.tool_name,
                    "failed": nested_result.failed, "output": nested_result.output,
                    "output_truncated": nested_result.output_truncated,
                    "recovery_available": !nested_result.output_truncated,
                })
            });
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


    fn finish_packet(&self, cell_id: &str, retain_terminal: bool) -> CodeModePacketReceipt {
        self.restore_terminal_packet(cell_id);
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
                metrics.receipt_permit = packet.receipt_permit.take();
                metrics.terminal_parent_call_id = packet.terminal_parent_call_id.take();
                // A steered observer may leave storage or native callbacks
                // pending. Keep their reservations until an observed drain;
                // a slow disk must not let a new packet over-admit effects.
                metrics.recovery_metadata_bytes = if packet
                    .retention_tail
                    .as_ref()
                    .is_some_and(|tail| tail.peek().is_none())
                {
                    packet.recovery_metadata_bytes
                } else {
                    packet.pending_recovery_metadata.values().sum()
                };
                metrics.pending_recovery_metadata =
                    std::mem::take(&mut packet.pending_recovery_metadata);
                // Unsettled calls stay named until they settle; the receipt
                // keeps a snapshot for a terminal response to report.
                metrics.pending_call_identities = packet.pending_call_identities.clone();
                metrics.retention_tail = packet.retention_tail.take();
                // Live output and steering can cross outstanding child calls.
                // Drain response data, but keep registration order for the cell.
                metrics.next_nested_ordinal = next_nested_ordinal;
                metrics.output_budget = packet.output_budget;
                metrics.turn_id = packet.turn_id.take();
                metrics.delivery_retired = packet.delivery_retired;
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
            post_tool_use_feedback: metrics.post_tool_use_feedback,
            nested_results: metrics.nested_results,
            nested_recovery: metrics.nested_recovery,
            omitted_nested_result_count: metrics.omitted_nested_result_count,
            first_required_terminal: metrics.first_required_terminal,
            tool_output_recovery_count: metrics.tool_output_recovery_count,
            unsettled_nested_calls: metrics.pending_call_identities.into_values().collect(),
            unobserved_settlements: metrics.unobserved_settlements,
            omitted_unobserved_settlement_count: metrics.omitted_unobserved_settlement_count,
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
        self.seal_completed_cells();
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

fn refresh_ready_command_states(exec: &ExecContext, packet: &mut CodeModePacketReceipt) {
    use codex_tools::ToolOutput;
    for state in &mut packet.command_states {
        let Some(process_id) = state["session_id"].as_u64().and_then(|id| u32::try_from(id).ok()) else { continue; };
        let Some(incarnation) = state.pointer("/session_capabilities/incarnation")
            .and_then(JsonValue::as_str).and_then(|id| uuid::Uuid::parse_str(id).ok()) else { continue; };
        let Some(result) = exec.session.services.unified_exec_manager.try_finish_for_packet(
            process_id, incarnation, exec.turn.model_info.truncation_policy.into(),
            Some(codex_code_mode::MAX_NESTED_COMMAND_OUTPUT_TOKENS),
        ) else { continue; };
        let result = result.code_mode_result(&ToolPayload::Function { arguments: "{}".into() });
        let Some(mut refreshed) = command_result_state(&result) else { continue; };
        for key in ["call_id", "tool", "polled_session_id", "pending_deferred_completions"] {
            if let Some(value) = state.get(key) { refreshed[key] = value.clone(); }
        }
        packet.post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
            text: serde_json::json!({"nested_command_settlement": {
                "call_id": state["call_id"], "result": result,
                "notice": "This ready-only owner settlement supersedes the earlier command snapshot; no command was rerun.",
            }}).to_string(),
        });
        *state = refreshed;
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
    let mut packet = exec.session.services.code_mode_service.finish_packet(
        cell_id,
        !runtime_response_is_terminal(&response),
    );
    refresh_ready_command_states(exec, &mut packet);
    tracing::info!(
        target: "codex.code_mode.packet",
        cell_id,
        post_tool_use_feedback_count = packet.post_tool_use_feedback.len(),
        retained_nested_result_count = packet.nested_results.len(),
        omitted_nested_result_count = packet.omitted_nested_result_count,
        "code-mode packet admission receipt"
    );
    let mut post_tool_use_feedback = packet.post_tool_use_feedback;
    let cancelled_nested_calls = matches!(&response, RuntimeResponse::Terminated { .. }).then(|| {
        let mut calls = packet.nested_recovery.values().map(|entry| {
            // Reuse the existing exact-artifact index instead of placing a
            // second full successful read into non-spillable native metadata.
            let mut entry = compact_emitted_packet_recovery(entry.clone(), "");
            let failed = entry["failed"].as_bool().or_else(|| packet.nested_results.iter()
                .find(|result| Some(result.call_id.as_str()) == entry["call_id"].as_str())
                .map(|result| result.failed));
            let aborted = entry.pointer("/outcome/result/status").and_then(JsonValue::as_str) == Some("aborted");
            entry["disposition"] = match failed.filter(|_| !aborted) {
                Some(false) => "settled_success", Some(true) => "settled_failure", None => "unresolved",
            }.into();
            entry
        }).collect::<Vec<_>>();
        calls.extend(packet.unsettled_nested_calls.iter().map(|entry| {
            let mut entry = entry.clone();
            entry["disposition"] = "unresolved".into();
            entry
        }));
        serde_json::json!({"calls":calls,"retry_safe":false})
    });
    let needs_retained_results = response_needs_retained_nested_results(&response);
    let nested_results = if needs_retained_results {
        if packet.omitted_nested_result_count > 0
            || packet
                .nested_results
                .iter()
                .any(|result| result.output_truncated)
        {
            let emitted = match &response {
                RuntimeResponse::Result { content_items, .. }
                | RuntimeResponse::Terminated { content_items, .. }
                | RuntimeResponse::Yielded { content_items, .. }
                | RuntimeResponse::ExplicitYield { content_items, .. } => code_mode_text_content(
                    &into_function_call_output_content_items(content_items.clone()),
                ),
            };
            post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
                text: serde_json::json!({
                    "nested_result_recovery_directory": packet.nested_recovery.into_values().map(|entry| compact_emitted_packet_recovery(entry, &emitted)).collect::<Vec<_>>(),
                    "omitted_inline_result_count": packet.omitted_nested_result_count,
                    "notice": "Inline diagnostics retain at most two results. Recover settled outcomes through this directory; do not rerun successful siblings. Artifact bytes are historical evidence, not fresh execution.",
                }).to_string(),
            });
        }
        packet.nested_results
    } else {
        Vec::new()
    };
    // The script handled what it observed. A cancelled wait observed only an
    // error, so the owner's settled outcome must still reach the model. Cells
    // that keep retained results already carry it in those results.
    if !needs_retained_results {
        post_tool_use_feedback.extend(packet.unobserved_settlements);
        if packet.omitted_unobserved_settlement_count > 0 {
            post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
                text: serde_json::json!({
                    "omitted_unobserved_nested_settlements": packet.omitted_unobserved_settlement_count,
                    "retry_safe": false,
                })
                .to_string(),
            });
        }
    }
    // A finished script is not the outcome of nested calls it stopped waiting
    // for. Name them so the model inspects their effects instead of replaying.
    if !matches!(
        &response,
        RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. }
    ) && !packet.unsettled_nested_calls.is_empty()
    {
        post_tool_use_feedback.push(FunctionCallOutputContentItem::InputText {
            text: serde_json::json!({"unsettled_nested_calls": {
                "count": packet.unsettled_nested_calls.len(),
                "calls": packet
                    .unsettled_nested_calls
                    .iter()
                    .take(MAX_LISTED_UNSETTLED_NESTED_CALLS)
                    .collect::<Vec<_>>(),
                "retry_safe": false,
                "notice": "These nested calls had not settled when the cell ended; the script saw at most a timeout or cancellation error. Their effects are unknown: inspect current state before retrying them, and do not replay the cell.",
            }})
            .to_string(),
        });
    }
    // Host lifecycle state stays in the canonical record. Only live handles
    // need a separate inline receipt when the script did not print them.
    let canonical_states = packet.command_states;
    let completed_cleanly = matches!(
        &response,
        RuntimeResponse::Result {
            error_text: None,
            output_loss: None,
            ..
        }
    );
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
                }})
                .to_string(),
            });
        }
    }
    // Reserve the controls before admitting ordinary text to the shared budget.
    // Controls and the failure itself remain visible even at a zero text budget.
    let control_tokens = codex_utils_output_truncation::model_token_count(&code_mode_text_content(
        &command_control_receipts(&canonical_states, "", completed_cleanly),
    ));
    let diagnostic_tokens = match &response {
        RuntimeResponse::Result {
            error_text: Some(error),
            ..
        } => codex_utils_output_truncation::model_token_count(&format!("Script error:\n{error}")),
        _ => 0,
    };
    let requested = max_output_tokens
        .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        .min(hard_limit);
    let text_budget = requested
        .saturating_sub(control_tokens)
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
    // The cell budget is the only projection in-cell recovery crosses after
    // its own bounded read, so a truncated delivery reduced that evidence again.
    if packet.tool_output_recovery_count > 0
        && output.essential_inline.get(VISIBLE_OUTPUT_TRUNCATED_KEY) == Some(&JsonValue::Bool(true))
    {
        exec.turn
            .turn_timing_state
            .record_tool_output_recovery_retruncation();
    }
    if completed_cleanly && !needs_retained_results {
        // The script consumed its recovery results and emitted its own result.
        // Keep drain accounting without appending raw recovered pages to that
        // computed answer. Failed, silent, and yielded cells retain the fallback.
        let service = &exec.session.services.code_mode_service;
        let receipts = service
            .owner_drained_continuation_snapshot(&completed_cell_id)
            .into_iter()
            .map(|continuation| continuation.receipt)
            .collect::<Vec<_>>();
        service.acknowledge_owner_drained_continuations(&completed_cell_id, &receipts);
        output.deterministic_continuation_receipts.extend(receipts);
    }
    output.body.extend(command_control_receipts(
        &canonical_states,
        &code_mode_text_content(&output.body),
        completed_cleanly,
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
    if let Some(calls) = cancelled_nested_calls {
        output.essential_inline.insert("cancelled_nested_calls".into(), calls);
    }
    Ok(output)
}

/// Host observation is not a rollback. Route failure through packet formatting
/// so known effects and outstanding call identities survive observer cleanup.
fn observation_error_response(cell_id: &CellId, error: String) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: cell_id.clone(),
        content_items: Vec::new(),
        error_text: Some(format!("Host observation failed: {error}. The enclosing cell state is uncertain; completed nested effects were not rolled back. Do not replay the cell.")),
        output_loss: None,
    }
}

fn observation_failed(response: &RuntimeResponse) -> bool {
    matches!(response, RuntimeResponse::Result{error_text:Some(error),..} if error.starts_with("Host observation failed:"))
}

fn runtime_response_is_terminal(response: &RuntimeResponse) -> bool {
    matches!(
        response,
        RuntimeResponse::Terminated { .. } | RuntimeResponse::Result { .. }
    ) && !observation_failed(response)
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
        if command_owns_work(state) && !shows_session_handle(visible, state) {
            items.push(FunctionCallOutputContentItem::InputText {
                text: format!("Running command session_id: {}", state["session_id"]),
            });
            if state["session_capabilities"].is_object() {
                items.push(FunctionCallOutputContentItem::InputText {
                    text: serde_json::json!({
                        "session_id": state["session_id"],
                        "session_capabilities": state["session_capabilities"],
                        "continuation": state["continuation"],
                    })
                    .to_string(),
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
        } if observation_failed(response) => (
            cell_id.as_str(),
            "in_progress",
            DynamicToolCallStatus::InProgress,
            Some(bounded_failed_cell_error(error)),
        ),
        RuntimeResponse::Result {
            cell_id,
            error_text: Some(error),
            ..
        } => (
            cell_id.as_str(),
            "failed",
            DynamicToolCallStatus::Failed,
            Some(bounded_failed_cell_error(error)),
        ),
        RuntimeResponse::Terminated { cell_id, .. } => (
            cell_id.as_str(),
            "terminated",
            DynamicToolCallStatus::Failed,
            Some("code-mode cell terminated before completion".to_string()),
        ),
        RuntimeResponse::Yielded { cell_id, .. } => (
            cell_id.as_str(),
            "in_progress",
            DynamicToolCallStatus::InProgress,
            None,
        ),
        RuntimeResponse::ExplicitYield { cell_id, .. } => (
            cell_id.as_str(),
            "yielded",
            DynamicToolCallStatus::InProgress,
            None,
        ),
        RuntimeResponse::Result {
            cell_id,
            error_text: None,
            ..
        } => (
            cell_id.as_str(),
            "completed",
            DynamicToolCallStatus::Completed,
            None,
        ),
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

fn attach_pending_retention(output: &mut FunctionToolOutput, cell_id: &CellId, call_id: &str) {
    let receipt = serde_json::json!({"canonical_retention": {
        "status": "pending", "cell_id": cell_id.as_str(), "observation_call_id": call_id,
        "retry_safe": false,
        "notice": "The terminal response is captured, but accepted canonical storage has not settled. Its existing writer still owns the evidence; do not replay external effects."
    }});
    output.essential_inline.insert(
        "canonical_retention".into(),
        receipt["canonical_retention"].clone(),
    );
    let item = FunctionCallOutputContentItem::InputText {
        text: receipt.to_string(),
    };
    output.body.push(item.clone());
    if let Some(canonical) = &mut output.canonical_body {
        canonical.push(item);
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
        // Progress and synthetic activity are not acknowledgment of nested
        // effects. Hand their receipts to the model before draining the packet.
        RuntimeResponse::Yielded { .. } | RuntimeResponse::ExplicitYield { .. } => true,
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
                })
                .to_string(),
            }
        })
        .collect()
}

/// Keep an index entry, not a second copy, of a settled successful result.
/// A result naming its own complete retained artifact becomes that artifact
/// reference whether or not the script printed it: text()-rendered bodies and
/// store()d results never contain the exact JSON, so failed cells re-embedded
/// such read_file bodies verbatim. Any other result is referenced only when
/// its exact JSON was printed; that output remains in the canonical cell artifact.
fn compact_emitted_packet_recovery(mut entry: JsonValue, emitted: &str) -> JsonValue {
    if entry["failed"] != false {
        return entry;
    }
    let Some(result) = entry.pointer("/outcome/result") else {
        return entry;
    };
    if let Some(reference) = retained_artifact_reference(result) {
        entry["outcome"] = reference;
        return entry;
    }
    if emitted.is_empty() {
        return entry;
    }
    let serialized = result.to_string();
    if serialized.len() >= 32 && emitted.contains(&serialized) {
        entry["outcome"] = serde_json::json!({
            "already_emitted_in_cell": true,
            "canonical_sha256": hex::encode(Sha256::digest(serialized.as_bytes())),
            "canonical_bytes": serialized.len(),
            "notice": "Exact result appears earlier in this cell output and its canonical artifact; do not replay the call.",
        });
    }
    entry
}

/// The recovery route of a result whose own retained artifact holds its bytes.
/// An explicitly incomplete retention cannot stand in for the result.
fn retained_artifact_reference(result: &JsonValue) -> Option<JsonValue> {
    let artifact_id = result
        .get("artifact_id")
        .and_then(JsonValue::as_str)
        .filter(|id| !id.is_empty())?;
    if result.get("retained_artifact_complete") == Some(&JsonValue::Bool(false)) {
        return None;
    }
    // A locator alone is not a callable recovery recipe. Reuse the producer's
    // exact selectors: command logs support lines, not arbitrary byte ranges,
    // and a selected source read need not authorize recovering the whole file.
    let results = result.get("results")?.as_array()?;
    if results.is_empty()
        || results.len() > crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS
    {
        return None;
    }
    let selectors = results
        .iter()
        .map(|result| {
            serde_json::from_value::<crate::tools::command_output_artifact::ToolOutputSelector>(
                result.get("selector")?.clone(),
            )
            .ok()
        })
        .collect::<Option<Vec<_>>>()?;
    let mut reference = serde_json::json!({
        "artifact_id": artifact_id,
        "recovery": {"tool": "read_tool_output", "arguments": {
            "artifact_id": artifact_id, "selectors": selectors,
        }},
    });
    if let Some(sha256) = result.get("canonical_sha256").and_then(JsonValue::as_str) {
        reference["canonical_sha256"] = sha256.into();
    }
    if let Some(bytes) = result.get("canonical_bytes").and_then(JsonValue::as_u64) {
        reference["canonical_bytes"] = bytes.into();
    }
    Some(reference)
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
    let execution_unknown = observation_failed(&response);
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
    let store_commit_failure = script_error
        .as_deref()
        .and_then(|error| serde_json::from_str::<JsonValue>(error).ok())
        .filter(|receipt| {
            receipt.get("kind").and_then(JsonValue::as_str)
                == Some("code_mode_store_commit_failure")
                && receipt.get("version").and_then(JsonValue::as_u64) == Some(1)
        })
        .map(|mut receipt| {
            receipt["automatic_replay_allowed"] = JsonValue::Bool(false);
            receipt["observed_nested_call_ids"] = serde_json::json!(
                nested_results
                    .iter()
                    .map(|result| &result.call_id)
                    .collect::<Vec<_>>()
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
    let total_lines = line_offset
        + code_mode_text_content(&canonical_content_items)
            .lines()
            .count();
    let (mut content_items, visible_output_truncated, omitted_lines) =
        truncate_code_mode_result_at_lines(
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
        output
            .essential_inline
            .insert("store_commit".to_string(), receipt);
    }
    if execution_unknown {
        output.essential_inline.insert("cell_execution_state".into(), "unknown".into());
        output.essential_inline.insert("retry_safe".into(), false.into());
    }
    if visible_output_truncated {
        output.essential_inline.insert(
            VISIBLE_OUTPUT_TRUNCATED_KEY.to_string(),
            JsonValue::Bool(true),
        );
        if let Some((start, end)) = omitted_lines {
            output.essential_inline.insert(
                "cell_output_recovery_selector".into(),
                serde_json::json!({"kind":"lines", "start":start, "end":end}),
            );
        }
    }
    match required_terminal {
        Some(terminal) => fold_nested_required_terminal(output, terminal),
        None => output,
    }
}

fn format_script_status(response: &RuntimeResponse) -> String {
    if observation_failed(response) {
        return format!("Cell execution state unknown after host observation failure (cell ID {})", runtime_response_cell_id(response));
    }
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


fn truncate_code_mode_result_at_lines(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
    outcome: OutputOutcome,
    hard_limit: usize,
    diagnostic_index: Option<usize>,
    source_lines: Option<(usize, usize)>,
) -> (
    Vec<FunctionCallOutputContentItem>,
    bool,
    Option<(usize, usize)>,
) {
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
    let whole_packet_limit = limits
        .applied_limit
        .saturating_add(limits.applied_limit / 3)
        .min(hard_limit);
    if diagnostic_index.is_none()
        && items.len() <= 1
        && items
            .iter()
            .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (offset, total) = source_lines.unwrap_or((0, diagnostic_text.lines().count()));
        let (truncated, omitted_lines) =
            codex_utils_output_truncation::truncate_model_text_at_lines_with_limits(
                &diagnostic_text,
                limits.applied_limit,
                whole_packet_limit,
                offset,
                total,
                Some("{cell_output_artifact_id}"),
            );
        let omitted = truncated != diagnostic_text;
        // Keep original item boundaries for a fitting packet, including empty
        // items. The zero-budget path retains its existing empty projection.
        if !omitted && limits.applied_limit != 0 {
            return (items, false, None);
        }
        return (
            vec![FunctionCallOutputContentItem::InputText { text: truncated }],
            omitted,
            omitted_lines,
        );
    }

    if limits.applied_limit != 0
        && codex_utils_output_truncation::model_token_count(&diagnostic_text) <= whole_packet_limit
    {
        return (items, false, None);
    }
    if let Some(error_index) = diagnostic_index {
        return truncate_code_mode_failure(items, error_index, limits.applied_limit, source_lines);
    }
    truncate_code_mode_text_items(items, limits.applied_limit, source_lines)
}

/// Preserve each registered text result and its position relative to media.
/// Complete small results yield their unused share before large ones truncate.
fn truncate_code_mode_text_items(
    items: Vec<FunctionCallOutputContentItem>,
    token_limit: usize,
    source_lines: Option<(usize, usize)>,
) -> (Vec<FunctionCallOutputContentItem>, bool, Option<(usize, usize)>) {
    let costs = items.iter().enumerate().filter_map(|(index, item)| match item {
        FunctionCallOutputContentItem::InputText { text } => {
            Some((index, codex_utils_output_truncation::model_token_count(text)))
        }
        _ => None,
    }).collect::<Vec<_>>();
    let mut order = costs.clone();
    order.sort_by_key(|(index, cost)| (*cost, *index));
    let mut budgets = vec![0; items.len()];
    let mut remaining = token_limit.saturating_sub(costs.len().saturating_sub(1));
    for (position, (index, cost)) in order.into_iter().enumerate() {
        budgets[index] = cost.min(remaining / (costs.len() - position));
        remaining -= budgets[index];
    }
    let (mut offset, total) = source_lines.unwrap_or_else(|| (0, code_mode_text_content(&items).lines().count()));
    let mut seen_text = false;
    let mut omitted = false;
    let mut omitted_lines: Option<(usize, usize)> = None;
    let projected = items.into_iter().enumerate().map(|(index, item)| {
        let FunctionCallOutputContentItem::InputText { text } = item else { return item };
        offset += usize::from(seen_text);
        let (projected, gap) = codex_utils_output_truncation::truncate_model_text_at_lines_with_recovery(
            &text, budgets[index], offset, total, Some("{cell_output_artifact_id}"),
        );
        omitted |= projected != text;
        if let Some((start, end)) = gap {
            omitted_lines = Some(match omitted_lines {
                Some((first, last)) => (first.min(start), last.max(end)),
                None => (start, end),
            });
        }
        offset += text.bytes().filter(|byte| *byte == b'\n').count();
        seen_text = true;
        FunctionCallOutputContentItem::InputText { text: projected }
    }).collect();
    (projected, omitted, omitted_lines)
}

fn truncate_code_mode_failure(
    mut items: Vec<FunctionCallOutputContentItem>,
    error_index: usize,
    token_limit: usize,
    source_lines: Option<(usize, usize)>,
) -> (
    Vec<FunctionCallOutputContentItem>,
    bool,
    Option<(usize, usize)>,
) {
    let FunctionCallOutputContentItem::InputText { text: error_text } = items.remove(error_index)
    else {
        unreachable!("the caller identifies a script-error text item")
    };
    let error_tokens = codex_utils_output_truncation::model_token_count(&error_text);
    let preceding = code_mode_text_content(&items);
    let (offset, total) =
        source_lines.unwrap_or((0, preceding.lines().count() + error_text.lines().count()));
    let error_offset = offset
        + preceding.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!items.is_empty());
    let reserved_error_tokens = error_tokens.min(token_limit);
    let other_policy = TruncationPolicy::Tokens(token_limit.saturating_sub(reserved_error_tokens));
    let (mut projected, mut omitted, mut omitted_lines) =
        truncate_code_mode_text_items(items, other_policy.token_budget(), Some((offset, total)));
    let error_text = if error_tokens <= reserved_error_tokens {
        error_text
    } else {
        omitted = true;
        let (text, gap) = codex_utils_output_truncation::truncate_model_text_at_lines_with_recovery(
            &error_text,
            reserved_error_tokens,
            error_offset,
            total,
            Some("{cell_output_artifact_id}"),
        );
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
    visible
        .lines()
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .any(|value| {
            let mut pending = vec![&value];
            while let Some(value) = pending.pop() {
                if value.get("session_id") == state.get("session_id")
                    && value["session_capabilities"].is_object()
                    && value.get("session_capabilities") == state.get("session_capabilities")
                    && (value["execution_state"] == "running"
                        || value.get("continuation").is_some_and(|continuation| {
                            !continuation.is_null()
                                && Some(continuation) == state.get("continuation")
                        }))
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
    visible
        .lines()
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .any(|value| {
            let mut pending = vec![&value];
            while let Some(value) = pending.pop() {
                let found = if let Some(recovery) = state.get("recovery") {
                    value.get("recovery") == Some(recovery)
                        || (value.get("raw_output_artifact_id")
                            == state.get("raw_output_artifact_id")
                            && value.get("recovery_selector").is_some()
                            && value.get("recovery_selector")
                                == recovery.pointer("/arguments/selectors/0"))
                } else {
                    state["raw_output_artifact_id"].is_string()
                        && (value.get("artifact_id") == state.get("raw_output_artifact_id")
                            || value.get("raw_output_artifact_id")
                                == state.get("raw_output_artifact_id"))
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
    let nested_call_id = format!(
        "{PUBLIC_TOOL_NAME}-{}-{runtime_tool_call_id}",
        cell_id.as_str()
    );
    let packet_ordinal = exec
        .session
        .services
        .code_mode_service
        .admit_packet_call(
            &cell_id,
            nested_call_id
                .len()
                .saturating_add(tool_name.to_string().len()),
        )
        .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
    exec.session
        .services
        .code_mode_service
        .record_packet_call_identity(
            &cell_id,
            packet_ordinal,
            &nested_call_id,
            &tool_name.to_string(),
        );
    if is_exec_tool_name(&tool_name) {
        let message = format!("{PUBLIC_TOOL_NAME} cannot invoke itself");
        exec.session
            .services
            .code_mode_service
            .complete_packet_call(
                &cell_id,
                packet_ordinal,
                Vec::new(),
                None,
                Some((RequiredToolTerminalCause::Failure, message.clone())),
            );
        return Err(FunctionCallError::RespondToModel(message));
    }

    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => {
            let (retained_output, output_truncated, _) = bounded_failed_nested_text(&error);
            tool_runtime
                .record_code_mode_failure(
                    cell_id.as_str(),
                    &tool_name,
                    None,
                    nested_failure_fingerprint(&tool_name, &error),
                )
                .await;
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
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
                        output: retained_output,
                        output_truncated,
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
        tool_runtime
            .record_code_mode_failure(
                cell_id.as_str(),
                &tool_name,
                Some(&payload),
                nested_failure_fingerprint(&tool_name, &message),
            )
            .await;
        exec.session
            .services
            .code_mode_service
            .complete_packet_call(
                &cell_id,
                packet_ordinal,
                Vec::new(),
                None,
                // No effect was dispatched. A caught routing error can be
                // repaired by the native tool; uncaught errors still fail JS.
                None,
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
            None,
        )
        .await;
    let mut result = match result {
        Ok(result) => result,
        Err(error) => {
            let message = error.to_string();
            let (retained_output, output_truncated, _) = bounded_failed_nested_text(&message);
            let terminal_cause = required_tool_error_terminal_cause(&error);
            tool_runtime
                .record_code_mode_failure(
                    cell_id.as_str(),
                    &tool_name,
                    None,
                    nested_failure_fingerprint(&tool_name, &message),
                )
                .await;
            exec.session
                .services
                .code_mode_service
                .complete_packet_call(
                    &cell_id,
                    packet_ordinal,
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
                        output: retained_output,
                        output_truncated,
                    }),
                    terminal_cause.map(|cause| (cause, message)),
                );
            return Err(error);
        }
    };
    // Dispatch applies pre-tool hooks. All settled evidence must describe the
    // invocation the owner actually executed, not the pre-hook request.
    let payload = result.payload.clone();
    let outcome_context = result.outcome_context();
    let mut signal = result.sampling_request_signal();
    if let Some(revision) = *admitted_revision
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        signal.get_or_insert_with(|| serde_json::json!({}))["validation_mutation_revision"] =
            serde_json::json!(revision);
    }
    let canonical_artifact_required = result.requires_canonical_artifact();
    let retained_artifact = result.canonical_delivery_artifact();
    let delivery_artifact = canonical_artifact_required
        .then(|| retained_artifact.clone())
        .flatten();
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
    let budget = exec
        .session
        .services
        .code_mode_service
        .output_budget(cell_id.as_str())
        .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        * 4
        / 5;

    let mut result_value = result.code_mode_result_with_budget(budget);
    if tool_name == ToolName::plain("tool_search") {
        annotate_search_execution_routes(&mut result_value, exec.turn.config.code_mode.excluded_tool_namespaces.iter());
    }
    tracing::info!(event.name = "codex.code_mode_nested_tool.semantic_result",
        turn_id = %exec.turn.sub_id, runtime_cell_id = %cell_id,
        runtime_tool_call_id = %runtime_tool_call_id,
        parent_tool_call_id = ?parent_tool_call_id,
        tool_name = %tool_name,
        semantic_outcome = ?outcome_context.outcome,
        exit_code = result_value.get("exit_code").and_then(JsonValue::as_i64),
        "owning tool classified nested result independently of transport completion");
    let recovery = exec
        .session
        .services
        .code_mode_service
        .record_delivery_evidence(
            &cell_id,
            canonical_artifact_required,
            delivery_artifact,
            signal.as_ref(),
            &result_value,
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
                result_value = JsonValue::String(format!(
                    "{text}\n{}",
                    serde_json::json!({"canonical_recovery": recovery})
                ));
            }
        }
    }
    if (tool_name == ToolName::plain("exec_command") || tool_name == ToolName::plain("write_stdin"))
        && result_value.get("output_reduced") == Some(&JsonValue::Bool(true))
    {
        exec.turn
            .turn_timing_state
            .record_nested_tool_output_reduction();
    }
    // A failed nested result carries its remediation at the end of the
    // message, so bound it from both ends; a successful one keeps the existing
    // head-only bound. The byte budget is the same either way.
    let nested_failed = !matches!(
        outcome_context.outcome,
        ToolOutputOutcome::Success | ToolOutputOutcome::Yielded
    );
    let (retained_output, output_truncated, result_bytes) = if nested_failed {
        bounded_failed_nested_json(&result_value)
    } else {
        bounded_serialized_json(&result_value)
    };
    // Source evidence may outlive the packet; retain its independent pin when
    // it cannot use a compact native read receipt or a small inline observation.
    let compact_source_read =
        tool_name == ToolName::plain("read_file") && result_value["source_sha256"].is_string();
    let needs_source_pin = parent_tool_call_id.is_some()
        && source_dependencies
            .as_ref()
            .is_some_and(|dependencies| !dependencies.is_empty())
        && result_bytes > 4_096
        && !compact_source_read;
    let mut recovery_artifact = retained_artifact;
    let inline_recovery = recovery_artifact.is_none() && !needs_source_pin
        && exec.session.services.code_mode_service.try_record_inline_packet_recovery(
            &cell_id, packet_ordinal, result_bytes, || serde_json::json!({
                "call_id":nested_call_id, "tool_name":tool_name.to_string(),
                "failed":!matches!(outcome_context.outcome, ToolOutputOutcome::Success | ToolOutputOutcome::Yielded),
                "outcome":{"result":result_value},
            }),
        );
    let defer_retention = recovery_artifact.is_none() && !inline_recovery;
    // Publish the capability before handing storage to an independent owner.
    // It remains usable after the runtime cell and packet state are retired.
    let retention_id = defer_retention.then(crate::tools::command_output_artifact::ToolOutputArtifactId::new);
    let retention_owner = retention_id.map(|id| {
        crate::tools::command_output_artifact::reserve_canonical_retention(
            &exec.turn.config.codex_home, &exec.session.thread_id.to_string(), id,
        )
    });
    // Reserve a transport-safe representation before the remote bridge tries
    // encoding. Raw result bytes remain canonical; transport never reruns the
    // producer or converts its completed success into a frame-size failure.
    let large_result_receipt = (result_bytes > 32 * 1024 * 1024).then(|| {
        let artifact = recovery_artifact.as_ref().map(|(id, sha256, bytes)| serde_json::json!({
            "artifact_id":id, "canonical_sha256":sha256,"canonical_bytes":bytes,
            "canonical_retention":"complete",
            "recovery":{"tool":"read_tool_output","arguments":{"artifact_id":id,
                "selectors":[{"kind":"bytes","start":0,"end":bytes}]}}
        })).unwrap_or_else(|| serde_json::json!({
            "artifact_id":retention_id.map(|id| id.to_string()), "canonical_bytes":result_bytes,
            "canonical_retention":"pending", "retry_safe":false,
            "recovery":{"tool":"read_tool_output","arguments":{"artifact_id":retention_id.map(|id| id.to_string()),
                "selectors":[{"kind":"bytes","start":0,"end":result_bytes}]}}
        }));
        serde_json::json!({"result_retained":true,"call_id":nested_call_id,
            "completed":outcome_context.outcome == ToolOutputOutcome::Success,
            "failed":nested_failed, "outcome":artifact,
            "notice":"The raw result is retained, not inline. Recover it with bounded read_tool_output selections; do not replay the tool."})
    });
    if let Some(id) = retention_id {
        exec.session.services.code_mode_service.record_packet_recovery(&cell_id, packet_ordinal,
            serde_json::json!({"call_id":nested_call_id,"tool_name":tool_name.to_string(),
                "outcome":{"artifact_id":id.to_string(),"canonical_retention":"pending",
                    "retry_safe":false,"recovery":{"tool":"read_tool_output","arguments":{
                        "artifact_id":id.to_string(),"selectors":[{"kind":"bytes","start":0,"end":result_bytes}]}}}}));
    }
    let retention = {
        let exec = exec.clone();
        let cell_id = cell_id.clone();
        let nested_call_id = nested_call_id.clone();
        let parent_tool_call_id = parent_tool_call_id.clone();
        let tool_name = tool_name.clone();
        let result_value = result_value.clone();
        let retained_output = retained_output.clone();
        let has_source_dependencies = source_dependencies
            .as_ref()
            .is_some_and(|dependencies| !dependencies.is_empty());
        let failed = !matches!(
            outcome_context.outcome,
            ToolOutputOutcome::Success | ToolOutputOutcome::Yielded
        );
        async move {
            if let Some(id) = retention_id {
                let canonical = codex_tools::CanonicalToolResult::json(result_value.clone());
                let artifact =
                    crate::tools::command_output_artifact::create_canonical_output_artifact_with_id(
                        &exec.turn.config.codex_home,
                        &exec.session.thread_id.to_string(),
                        &canonical,
                        id,
                    )
                    .await;
                if let Some(owner) = retention_owner {
                    owner.settle(&artifact);
                }
                if artifact.complete
                    && let Some(id) = artifact.artifact_id()
                {
                    exec.session
                        .register_tool_artifact_origin(
                            id.clone(),
                            nested_call_id.clone(),
                            canonical.exact_bytes,
                            canonical.sha256.clone(),
                        )
                        .await;
                    recovery_artifact = Some((id, canonical.sha256, canonical.exact_bytes));
                }
            }
            if !inline_recovery {
                let recovery = if let Some((id, sha256, bytes)) = &recovery_artifact {
                    serde_json::json!({
                        "artifact_id": id, "canonical_sha256": sha256, "canonical_bytes": bytes,
                        "recovery": {"tool":"read_tool_output", "arguments": {
                            "artifact_id":id, "selectors":[{"kind":"bytes","start":0,"end":bytes}]
                        }},
                    })
                } else {
                    serde_json::json!({"output": retained_output, "output_truncated":output_truncated,
                        "recovery_available":!output_truncated,
                        "notice":"Canonical retention failed; do not replay effects."})
                };
                let settlement = serde_json::json!({"call_id":nested_call_id,"tool_name":tool_name.to_string(),
                        "failed":failed,
                        "outcome":recovery});
                let delivered = exec.session.services.code_mode_service.record_packet_recovery(
                    &cell_id, packet_ordinal, settlement.clone());
                if !delivered {
                    // Do not recreate a closed runtime cell. The already-issued
                    // artifact id resolves successful storage; failures get an
                    // explicit terminal settlement rather than permanent pending.
                    exec.session.send_event(&exec.turn,
                        codex_protocol::protocol::EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                            message: format!("Settled retained result for closed Code Mode cell {cell_id}: {settlement}. Do not replay effects."),
                        })).await;
                }
            }
            if let Some(parent_call_id) = parent_tool_call_id.as_ref()
                && has_source_dependencies
            {
                let evidence_output = if result_bytes <= 4_096 {
                    Some(result_value.to_string())
                } else if compact_source_read {
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
        }
    };
    if defer_retention {
        let previous = exec
            .session
            .services
            .code_mode_service
            .queue_packet_retention(&exec.session, &cell_id, retention);
        if let Some(previous) = previous {
            tokio::select! {
                biased;
                _ = previous => {}
                _ = cancellation.token().cancelled() => {}
            }
        }
    } else {
        retention.await;
    }
    let script_result = large_result_receipt.unwrap_or_else(|| script_visible_nested_result(
        &tool_name,
        result_value.clone(),
        &nested_call_id,
        packet_ordinal,
    ));
    let nested_result = CodeModeNestedResultEvidence {
        failed: !matches!(
            outcome_context.outcome,
            codex_tools::ToolOutputOutcome::Success | codex_tools::ToolOutputOutcome::Yielded
        ),
        command_state: nested_command_state(
            tool_runtime.command_semantic_name(&tool_name).as_ref(),
            &nested_call_id,
            &payload,
            &result_value,
        ),
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
    let post_context_failure = signal
        .as_ref()
        .and_then(|signal| signal.get("post_tool_context_error").or_else(|| signal.get("required_accounting_error")))
        .and_then(JsonValue::as_str)
        .map(|message| (RequiredToolTerminalCause::Failure, message.to_string()));
    let required_terminal = post_context_failure.or_else(|| {
        required_nested_tool_terminal_cause(outcome_context, signal.as_ref())
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
            })
    });
    if cancellation.token().is_cancelled() {
        // The script's wait for this call ended first (wrapper deadline, cell
        // end or termination), so it observed only an error. Keep the owner's
        // settled outcome model-visible even when the script completes.
        exec.session
            .services
            .code_mode_service
            .record_unobserved_settlement(
                &cell_id,
                serde_json::json!({"unobserved_nested_settlement": {
                    "call_id": nested_result.call_id,
                    "tool_name": nested_result.tool_name,
                    "failed": nested_result.failed,
                    "output": nested_result.output,
                    "output_truncated": nested_result.output_truncated,
                    "retry_safe": false,
                    "notice": "The script's wait for this call was cancelled, so it saw only an error. This is the owner's settled outcome: inspect it before retrying, and do not replay effects it reports.",
                }}),
            );
    }
    exec.session
        .services
        .code_mode_service
        .complete_packet_call(
            &cell_id,
            packet_ordinal,
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

fn annotate_search_execution_routes<'a>(
    result: &mut JsonValue,
    excluded: impl Iterator<Item = &'a String>,
) {
    let excluded = excluded
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    let mut direct = std::collections::BTreeSet::new();
    if let Some(tools) = result["tools"].as_array() {
        for tool in tools {
            if let Some(namespace) =
                tool["name"].as_str().filter(|name| excluded.contains(name))
                && let Some(members) = tool["tools"].as_array()
            {
                for member in members {
                    if let Some(name) = member["name"].as_str() {
                        direct.insert(format!("{namespace}.{name}"));
                    }
                }
            }
        }
    }
    if let Some(names) = result["activated_omitted_tools"].as_array() {
        for name in names.iter().filter_map(JsonValue::as_str) {
            if name
                .split_once('.')
                .is_some_and(|(namespace, _)| excluded.contains(namespace))
            {
                direct.insert(name.to_string());
            }
        }
    }
    if !direct.is_empty() {
        result["direct_only_tools"] = serde_json::json!(direct);
        result["direct_only_guidance"] = serde_json::json!(
            "These tools were activated for the next direct model interface. They are excluded from this cell's immutable catalog: do not use resolve_tool for these names. End the cell and invoke their advertised direct schemas."
        );
    }
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
        for key in [
            "wall_time_seconds",
            "original_token_count",
            "original_token_count_is_approximate",
        ] {
            object.remove(key);
        }
        if object.contains_key("chunk_id") {
            let mut hash = Sha256::new();
            hash.update(b"command-chunk-v1\0");
            hash.update((call_id.len() as u64).to_be_bytes());
            hash.update(call_id.as_bytes());
            hash.update((position as u64).to_be_bytes());
            object.insert("chunk_id".to_string(), hex::encode(hash.finalize()).into());
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
#[expect(
    clippy::expect_used,
    reason = "serializing a string to an in-memory JSON string has no fallible keys or custom serializer; preserve exact escaped fingerprints"
)]
fn nested_output_fingerprints(
    tool: &ToolName,
    value: &JsonValue,
) -> Vec<(String, usize, [u8; 32])> {
    if tool.namespace.is_some()
        || !matches!(tool.name.as_str(), "exec_command" | "write_stdin")
        || value["repair"]
            .as_str()
            .is_some_and(|repair| !repair.is_empty())
    {
        return Vec::new();
    }
    let Some(output) = value["output"].as_str().filter(|output| output.len() >= 32) else {
        return Vec::new();
    };
    let escaped = serde_json::to_string(output).expect("string serialization");
    [output, &escaped[1..escaped.len() - 1]]
        .into_iter()
        .map(|text| {
            (
                text.chars().take(32).collect(),
                text.len(),
                Sha256::digest(text.as_bytes()).into(),
            )
        })
        .collect()
}

/// Whether the script already printed this retained result before the cell
/// ended. `text(result)` shows it JSON-escaped and `text(result.output)`
/// verbatim. Require the entire bounded payload, not matching ends: siblings
/// can share wrappers while carrying different diagnostics in the middle.
fn nested_result_already_emitted(result: &CodeModeNestedResultEvidence, emitted: &str) -> bool {
    if result
        .output_fingerprints
        .iter()
        .any(|(prefix, bytes, digest)| {
            // Repetition must not turn fallback deduplication into quadratic work.
            // Uncertain matches keep their diagnostic rather than dropping evidence.
            emitted.match_indices(prefix).take(32).any(|(start, _)| {
                emitted
                    .as_bytes()
                    .get(start..start.saturating_add(*bytes))
                    .is_some_and(|candidate| <[u8; 32]>::from(Sha256::digest(candidate)) == *digest)
            })
        })
    {
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
            .and_then(|raw| {
                codex_code_mode::model_visible_tool_result(
                    &ToolName::plain(&result.tool_name),
                    &raw,
                )
            })
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
            && result["streams_complete"] == true
            && result["exit_code"] == 0
            && result["process_exited"] == true
            && result["session_id"].is_null()
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
    result.get("process_exited")?;
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
        && let Some(incarnation) = result
            .pointer("/session_capabilities/incarnation")
            .and_then(JsonValue::as_str)
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
        "code_mode.nested_tool.{}",
        hex::encode(Sha256::digest(
            format!("{tool_name}\0{normalized}").as_bytes()
        ))
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



