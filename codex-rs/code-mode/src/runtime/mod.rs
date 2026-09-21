mod callbacks;
mod globals;
mod module_loader;
mod timers;
mod value;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::normalize_code_mode_identifier;
use codex_protocol::ToolName;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

const EXIT_SENTINEL: &str = "__codex_code_mode_exit__";
const RUNTIME_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_BUFFERED_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_ERROR_TEXT_BYTES: usize = 16 * 1024;
const ERROR_TRUNCATION_SUFFIX: &str = "\n[code mode error truncated]";
const OUTPUT_LIMIT_MESSAGE: &str =
    "Code mode output was truncated because the cell buffered more than 64 MiB.";

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    NotificationResponse { id: String },
    NotificationError { id: String, error_text: String },
    TimersReady,
    #[cfg(test)]
    InspectForTest { response: tokio::sync::oneshot::Sender<RuntimeInspection> },
    Terminate,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct RuntimeInspection {
    heap_bytes: usize,
    completion_collections: usize,
    stored_payload_address: usize,
    rust_conversion_bytes: usize,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    ContentItem {
        item: FunctionCallOutputContentItem,
        admitted_bytes: usize,
    },
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
        timeout_ms: u64,
    },
    Notify {
        id: Option<String>,
        call_id: String,
        text: String,
    },
    Result {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
        output_loss: Option<codex_code_mode_protocol::OutputLoss>,
    },
    ThreadPanicked,
}

pub(crate) struct OutputAdmission {
    state: Mutex<OutputAdmissionState>,
    max_bytes: usize,
    yield_pending: AtomicBool,
}

struct OutputAdmissionState {
    admitted_bytes: usize,
    overflow_reported: bool,
    output_loss: codex_code_mode_protocol::OutputLoss,
}

impl OutputAdmission {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(OutputAdmissionState {
                admitted_bytes: 0,
                overflow_reported: false,
                output_loss: codex_code_mode_protocol::OutputLoss::default(),
            }),
            max_bytes,
            yield_pending: AtomicBool::new(false),
        }
    }

    pub(super) fn admit_yield(&self) -> Option<RuntimeEvent> {
        // Repeated requests while the actor has not observed the first one have
        // the same effect. Keep synchronous JavaScript loops from flooding IPC.
        (!self.yield_pending.swap(true, Ordering::AcqRel)).then_some(RuntimeEvent::YieldRequested)
    }

    pub(crate) fn release_yield(&self) {
        self.yield_pending.store(false, Ordering::Release);
    }

    fn admit(&self, item: FunctionCallOutputContentItem) -> Option<RuntimeEvent> {
        let admitted_bytes = output_item_bytes(&item);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(next) = state
            .admitted_bytes
            .checked_add(admitted_bytes)
            .filter(|next| *next <= self.max_bytes)
        {
            state.admitted_bytes = next;
            return Some(RuntimeEvent::ContentItem {
                item,
                admitted_bytes,
            });
        }

        Self::record_loss(&mut state, admitted_bytes.saturating_sub(std::mem::size_of::<FunctionCallOutputContentItem>()))
    }

    fn record_loss(state: &mut OutputAdmissionState, bytes: usize) -> Option<RuntimeEvent> {
        state.output_loss.discarded_items = state.output_loss.discarded_items.saturating_add(1);
        state.output_loss.discarded_bytes_lower_bound = state.output_loss.discarded_bytes_lower_bound.saturating_add(bytes as u64);
        if !state.overflow_reported {
            state.overflow_reported = true;
            return Some(RuntimeEvent::ContentItem {
                item: FunctionCallOutputContentItem::InputText {
                    text: OUTPUT_LIMIT_MESSAGE.to_string(),
                },
                admitted_bytes: 0,
            });
        }
        None
    }

    pub(super) fn reject_before_conversion(&self, bytes: usize) -> Option<Option<RuntimeEvent>> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let required = bytes.saturating_add(std::mem::size_of::<FunctionCallOutputContentItem>());
        if required <= self.max_bytes.saturating_sub(state.admitted_bytes) { return None; }
        Some(Self::record_loss(&mut state, bytes))
    }

    fn output_loss(&self) -> Option<codex_code_mode_protocol::OutputLoss> {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.output_loss.discarded_items > 0).then(|| state.output_loss.clone())
    }

    pub(crate) fn release(&self, released_bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.admitted_bytes >= released_bytes);
        state.admitted_bytes = state.admitted_bytes.saturating_sub(released_bytes);
        if state.admitted_bytes == 0 {
            state.overflow_reported = false;
        }
    }
}

fn output_item_bytes(item: &FunctionCallOutputContentItem) -> usize {
    std::mem::size_of::<FunctionCallOutputContentItem>()
        .saturating_add(match item {
            FunctionCallOutputContentItem::InputText { text } => text.len(),
            FunctionCallOutputContentItem::InputImage { image_url, .. } => image_url.len(),
        })
        .max(1)
}

#[cfg(test)]
pub(crate) struct StartupTestGate {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: std::sync::Mutex<std_mpsc::Receiver<()>>,
    pub(crate) exited: tokio::sync::Notify,
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static STARTUP_TEST_GATE: Arc<StartupTestGate>;
}

#[cfg(test)]
struct StartupTestExit(Arc<StartupTestGate>);

#[cfg(test)]
impl Drop for StartupTestExit {
    fn drop(&mut self) {
        self.0.exited.notify_one();
    }
}

pub(crate) async fn spawn_runtime(
    stored_values: HashMap<String, JsonValue>,
    request: ExecuteRequest,
    default_tool_timeout_ms: u64,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    output_admission: Arc<OutputAdmission>,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<(std_mpsc::Sender<RuntimeCommand>, RuntimeTerminationHandle), String> {
    let (command_tx, command_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let (isolate_handle_tx, isolate_handle_rx) = tokio::sync::oneshot::channel();
    let ExecuteRequest {
        tool_call_id,
        enabled_tools,
        source,
        ..
    } = request;
    let enabled_tools = Arc::new(EnabledToolCatalog::new(
        enabled_tools
            .into_iter()
            .map(|definition| EnabledToolMetadata {
                global_name: normalize_code_mode_identifier(&definition.name),
                tool_name: definition.tool_name,
                description: definition.description,
                kind: definition.kind,
                default_timeout_ms: definition.default_timeout_ms,
            })
            .collect(),
    )?);
    let config = RuntimeConfig {
        tool_call_id,
        enabled_tools,
        source,
        stored_values,
        default_tool_timeout_ms,
        output_admission,
    };

    #[cfg(test)]
    let startup_test_gate = STARTUP_TEST_GATE.try_with(Arc::clone).ok();
    spawn_supervised_runtime_thread(event_tx.clone(), task_failure_handler, move || {
        #[cfg(test)]
        let _startup_test_exit = startup_test_gate.map(|gate| {
            gate.entered.notify_one();
            gate.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            StartupTestExit(gate)
        });
        if let Err(error) = ensure_v8_initialized() {
            let _ = isolate_handle_tx.send(Err(error));
            return;
        }
        run_runtime(
            config,
            event_tx,
            command_rx,
            isolate_handle_tx,
            runtime_command_tx,
        );
    });

    let isolate_handle =
        receive_runtime_startup(isolate_handle_rx, RUNTIME_STARTUP_TIMEOUT).await?;
    Ok((command_tx, isolate_handle.into_inner()))
}

// If startup times out or its caller is cancelled after the runtime sends a handle,
// dropping the unclaimed message must stop user code on that runtime thread.
struct StartupIsolateHandle(Option<RuntimeTerminationHandle>);

pub(crate) struct RuntimeTerminationHandle {
    isolate: v8::IsolateHandle,
    command_tx: std_mpsc::Sender<RuntimeCommand>,
}

impl RuntimeTerminationHandle {
    pub(crate) fn terminate_execution(&self) -> bool {
        let terminated = self.isolate.terminate_execution();
        let _ = self.command_tx.send(RuntimeCommand::Terminate);
        terminated
    }
}

impl StartupIsolateHandle {
    #[expect(
        clippy::expect_used,
        reason = "the private startup handle is consumed exactly once"
    )]
    fn into_inner(mut self) -> RuntimeTerminationHandle {
        self.0.take().expect("startup isolate handle")
    }
}

impl Drop for RuntimeTerminationHandle {
    fn drop(&mut self) {
        self.terminate_execution();
    }
}

async fn receive_runtime_startup<T>(
    receiver: tokio::sync::oneshot::Receiver<Result<T, String>>,
    timeout: Duration,
) -> Result<T, String> {
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(value)) => value,
        Err(_) => Err(format!(
            "code mode runtime initialization exceeded its {}ms timeout",
            timeout.as_millis()
        )),
        Ok(Err(_)) => Err("failed to initialize code mode runtime".to_string()),
    }
}

fn spawn_supervised_runtime_thread(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
    runtime: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(runtime)) {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic payload");
            if let Some(task_failure_handler) = task_failure_handler {
                task_failure_handler(format!("code-mode V8 runtime thread panicked: {message}"));
            }
            let _ = event_tx.send(RuntimeEvent::ThreadPanicked);
        }
    });
}

struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Arc<EnabledToolCatalog>,
    source: String,
    stored_values: HashMap<String, JsonValue>,
    default_tool_timeout_ms: u64,
    output_admission: Arc<OutputAdmission>,
}

#[derive(Default)]
struct EnabledToolCatalog {
    tools: Vec<EnabledToolMetadata>,
    by_global_name: HashMap<String, usize>,
}

impl EnabledToolCatalog {
    fn new(tools: Vec<EnabledToolMetadata>) -> Result<Self, String> {
        let mut by_global_name: HashMap<String, usize> = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            if let Some(existing_index) = by_global_name.get(&tool.global_name) {
                let existing = &tools[*existing_index];
                return Err(format!(
                    "code mode tool identifier collision: `{}` and `{}` both normalize to `{}`",
                    existing.tool_name, tool.tool_name, tool.global_name
                ));
            }
            by_global_name.insert(tool.global_name.clone(), index);
        }
        Ok(Self {
            tools,
            by_global_name,
        })
    }

    fn get(&self, index: usize) -> Option<&EnabledToolMetadata> {
        self.tools.get(index)
    }

    fn resolve(&self, global_name: &str) -> Option<&EnabledToolMetadata> {
        self.by_global_name
            .get(global_name)
            .and_then(|index| self.tools.get(*index))
    }

    fn index_of(&self, global_name: &str) -> Option<usize> {
        self.by_global_name.get(global_name).copied()
    }
}

pub(super) struct RuntimeState {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_notifications: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    unhandled_rejections: Vec<v8::Global<v8::Promise>>,
    rejection_tracking_overflow: bool,
    stored_values: HashMap<String, JsonValue>,
    stored_value_bytes: HashMap<String, usize>,
    total_stored_value_bytes: usize,
    stored_value_writes: HashMap<String, JsonValue>,
    stored_value_limit_error: Option<String>,
    #[cfg(test)]
    completion_collections: usize,
    #[cfg(test)]
    rust_conversion_bytes: usize,
    enabled_tools: Arc<EnabledToolCatalog>,
    next_tool_call_id: u64,
    next_notification_id: u64,
    next_timeout_id: u64,
    default_tool_timeout_ms: u64,
    tool_call_id: String,
    timer_scheduler: timers::TimerScheduler,
    exit_requested: bool,
    output_admission: Arc<OutputAdmission>,
}

pub(crate) const MAX_OUTSTANDING_CALLBACKS_PER_CELL: usize = 128;
pub(crate) const MAX_SESSION_STORED_VALUES: usize = 256;
pub(crate) const MAX_SESSION_STORED_VALUE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct StoredValueByteCounter {
    bytes: usize,
}

impl std::io::Write for StoredValueByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn stored_value_entry_bytes(key: &str, value: &JsonValue) -> usize {
    let mut counter = StoredValueByteCounter::default();
    if serde_json::to_writer(&mut counter, key).is_err()
        || serde_json::to_writer(&mut counter, value).is_err()
    {
        return usize::MAX;
    }
    counter.bytes
}

pub(crate) fn stored_values_with_writes_within_limits(
    current: &HashMap<String, JsonValue>,
    writes: &HashMap<String, JsonValue>,
) -> bool {
    let entry_count = current.len().saturating_add(
        writes
            .keys()
            .filter(|key| !current.contains_key(*key))
            .count(),
    );
    if entry_count > MAX_SESSION_STORED_VALUES {
        return false;
    }

    current
        .iter()
        .filter(|(key, _)| !writes.contains_key(*key))
        .chain(writes.iter())
        .try_fold(0usize, |total, (key, value)| {
            total.checked_add(stored_value_entry_bytes(key, value))
        })
        .is_some_and(|bytes| bytes <= MAX_SESSION_STORED_VALUE_BYTES)
}

pub(crate) fn stored_value_limit_message() -> String {
    format!(
        "code mode session storage exceeds its limit of {MAX_SESSION_STORED_VALUES} entries or {MAX_SESSION_STORED_VALUE_BYTES} serialized bytes"
    )
}

impl RuntimeState {
    pub(super) fn can_admit_callback(&self) -> bool {
        self.pending_tool_calls.len() + self.pending_notifications.len()
            < MAX_OUTSTANDING_CALLBACKS_PER_CELL
    }

    pub(super) fn emit_output(&self, item: FunctionCallOutputContentItem) {
        if let Some(event) = self.output_admission.admit(item) {
            let _ = self.event_tx.send(event);
        }
    }

    pub(super) fn stored_value_completion(&mut self) -> (HashMap<String, JsonValue>, Option<String>) {
        #[cfg(test)]
        { self.completion_collections += 1; }
        match self.stored_value_limit_error.as_ref() {
            Some(error) => (HashMap::new(), Some(error.clone())),
            None => (std::mem::take(&mut self.stored_value_writes), None),
        }
    }
}

pub(super) enum CompletionState {
    Pending,
    Completed {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
}

fn run_runtime(
    config: RuntimeConfig,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    isolate_handle_tx: tokio::sync::oneshot::Sender<Result<StartupIsolateHandle, String>>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    if isolate_handle_tx.is_closed() {
        return;
    }
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
    let isolate_handle = isolate.thread_safe_handle();
    if isolate_handle_tx
        .send(Ok(StartupIsolateHandle(Some(RuntimeTerminationHandle {
            isolate: isolate_handle,
            command_tx: runtime_command_tx.clone(),
        }))))
        .is_err()
    {
        return;
    }
    isolate.set_host_import_module_dynamically_callback(module_loader::dynamic_import_callback);
    isolate.set_promise_reject_callback(module_loader::promise_rejected);

    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let timer_scheduler = timers::TimerScheduler::new(runtime_command_tx);
    let stored_value_bytes: HashMap<_, _> = config
        .stored_values
        .iter()
        .map(|(key, value)| (key.clone(), stored_value_entry_bytes(key, value)))
        .collect();
    let total_stored_value_bytes = stored_value_bytes
        .values()
        .fold(0usize, |total, bytes| total.saturating_add(*bytes));
    scope.set_slot(RuntimeState {
        event_tx: event_tx.clone(),
        pending_tool_calls: HashMap::new(),
        pending_notifications: HashMap::new(),
        pending_timeouts: HashMap::new(),
        unhandled_rejections: Vec::new(),
        rejection_tracking_overflow: false,
        stored_values: config.stored_values,
        stored_value_bytes,
        total_stored_value_bytes,
        stored_value_writes: HashMap::new(),
        stored_value_limit_error: None,
        #[cfg(test)]
        completion_collections: 0,
        #[cfg(test)]
        rust_conversion_bytes: 0,
        enabled_tools: config.enabled_tools,
        next_tool_call_id: 1,
        next_notification_id: 1,
        next_timeout_id: 1,
        default_tool_timeout_ms: config.default_tool_timeout_ms,
        tool_call_id: config.tool_call_id,
        timer_scheduler,
        exit_requested: false,
        output_admission: config.output_admission,
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_result(scope, &event_tx, HashMap::new(), Some(error_text));
        return;
    }

    let _ = event_tx.send(RuntimeEvent::Started);

    let pending_promise = match {
        v8::scope!(let scope, scope);
        module_loader::evaluate_main_module(scope, &config.source)
    } {
        Ok(pending_promise) => pending_promise,
        Err(error_text) => {
            capture_scope_send_error(scope, &event_tx, Some(error_text));
            return;
        }
    };

    match module_loader::completion_state(scope, pending_promise.as_ref()) {
        CompletionState::Completed {
            stored_value_writes,
            error_text,
        } => {
            send_result(scope, &event_tx, stored_value_writes, error_text);
            return;
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Ok(command) = command_rx.recv() {
        v8::scope!(let scope, scope);
        match command {
            RuntimeCommand::Terminate => break,
            #[cfg(test)]
            RuntimeCommand::InspectForTest { response } => {
                scope.low_memory_notification();
                let heap_bytes = scope.get_heap_statistics().used_heap_size();
                let state = scope.get_slot::<RuntimeState>().unwrap();
                let _ = response.send(RuntimeInspection {
                    heap_bytes,
                    completion_collections: state.completion_collections,
                    rust_conversion_bytes: state.rust_conversion_bytes,
                    stored_payload_address: state.stored_value_writes.get("payload")
                        .and_then(JsonValue::as_str).map(|value| value.as_ptr() as usize).unwrap_or(0),
                });
            }
            RuntimeCommand::ToolResponse { id, result } => {
                if let Err(error_text) =
                    module_loader::resolve_tool_response(scope, &id, Ok(result))
                {
                    capture_scope_send_error(scope, &event_tx, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_tool_response(scope, &id, Err(error_text))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::NotificationResponse { id } => {
                if let Err(runtime_error) =
                    module_loader::resolve_notification_response(scope, &id, Ok(()))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::NotificationError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_notification_response(scope, &id, Err(error_text))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::TimersReady => {
                let fired = scope.get_slot::<RuntimeState>()
                    .map(|state| state.timer_scheduler.take_fired()).unwrap_or_default();
                for id in fired {
                    match timers::invoke_timeout_callback(scope, id) {
                        Ok(ControlFlow::Continue(())) => {}
                        Ok(ControlFlow::Break(())) => {
                            capture_scope_send_error(scope, &event_tx, None);
                            return;
                        }
                        Err(runtime_error) => {
                            capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                            return;
                        }
                    }
                    scope.perform_microtask_checkpoint();
                }
            }
        }

        scope.perform_microtask_checkpoint();
        match module_loader::completion_state(scope, pending_promise.as_ref()) {
            CompletionState::Completed {
                stored_value_writes,
                error_text,
            } => {
                send_result(scope, &event_tx, stored_value_writes, error_text);
                return;
            }
            CompletionState::Pending => {}
        }

        if let Some(promise) = pending_promise.as_ref() {
            let promise = v8::Local::new(scope, promise);
            if promise.state() != v8::PromiseState::Pending {
                pending_promise = None;
            }
        }
    }
}

fn capture_scope_send_error(
    scope: &mut v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    error_text: Option<String>,
) {
    let (stored_value_writes, stored_value_limit_error) = scope
        .get_slot_mut::<RuntimeState>()
        .map(RuntimeState::stored_value_completion)
        .unwrap_or_default();

    send_result(
        scope,
        event_tx,
        stored_value_writes,
        stored_value_limit_error.or(error_text),
    );
}

fn send_result(
    scope: &v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    stored_value_writes: HashMap<String, JsonValue>,
    error_text: Option<String>,
) {
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        output_loss: scope.get_slot::<RuntimeState>().and_then(|state| state.output_admission.output_loss()),
        error_text: error_text.map(|mut text| {
            if text.len() > MAX_ERROR_TEXT_BYTES {
                let mut end = MAX_ERROR_TEXT_BYTES - ERROR_TRUNCATION_SUFFIX.len();
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str(ERROR_TRUNCATION_SUFFIX);
            }
            text
        }),
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use codex_code_mode_protocol::CodeModeToolKind;
    use codex_code_mode_protocol::EnabledToolMetadata;
    use codex_protocol::ToolName;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::EnabledToolCatalog;
    use super::ExecuteRequest;
    use super::OUTPUT_LIMIT_MESSAGE;
    use super::OutputAdmission;
    use super::RuntimeEvent;
    use super::receive_runtime_startup;
    use super::spawn_runtime;
    use super::spawn_supervised_runtime_thread;

    fn execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
            default_tool_timeout_ms: None,
        }
    }

    #[test]
    fn enabled_tool_catalog_resolves_exact_names() {
        let metadata = |global_name: &str, description: &str| EnabledToolMetadata {
            tool_name: ToolName::plain(global_name),
            global_name: global_name.to_string(),
            default_timeout_ms: None,
            description: description.to_string(),
            kind: CodeModeToolKind::Function,
        };
        let catalog = EnabledToolCatalog::new(vec![
            metadata("sample_tool", "first"),
            metadata("sample_tool_extra", "other"),
        ])
        .expect("distinct global names should build a catalog");

        assert_eq!(
            catalog
                .resolve("sample_tool")
                .map(|tool| tool.description.as_str()),
            Some("first")
        );
        assert_eq!(
            catalog
                .resolve("sample_tool_extra")
                .map(|tool| tool.description.as_str()),
            Some("other")
        );
        assert!(catalog.resolve("sample").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_startup_receive_is_bounded_and_marks_late_initialization_cancelled() {
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        let error = receive_runtime_startup(startup_rx, Duration::from_millis(1))
            .await
            .expect_err("withheld startup handle should time out");

        assert_eq!(
            error,
            "code mode runtime initialization exceeded its 1ms timeout"
        );
        assert!(startup_tx.is_closed());
    }

    #[tokio::test]
    async fn runtime_startup_wait_allows_progress_on_a_current_thread_executor() {
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            startup_tx.send(Ok(42)).unwrap();
        });
        assert_eq!(
            receive_runtime_startup(startup_rx, Duration::from_secs(1)).await,
            Ok(42)
        );
    }

    #[tokio::test]
    async fn cancelling_runtime_startup_closes_the_handle_receiver() {
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let mut startup = Box::pin(receive_runtime_startup(startup_rx, Duration::from_secs(60)));
        assert!(futures::poll!(startup.as_mut()).is_pending());
        drop(startup);
        assert_eq!(startup_tx.send(Ok(())), Err(Ok(())));
    }

    #[tokio::test]
    async fn runtime_thread_panic_before_initialization_is_reported_directly() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        drop(event_rx);
        let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            Some(std::sync::Arc::new(move |reason| {
                let _ = failure_tx.send(reason);
            })),
            || panic!("runtime thread panic probe"),
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
                .await
                .expect("runtime failure timeout")
                .expect("runtime failure"),
            "code-mode V8 runtime thread panicked: runtime thread panic probe"
        );
    }

    #[tokio::test]
    async fn runtime_thread_panic_is_forwarded_without_owner_supervision() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            /*task_failure_handler*/ None,
            || panic!("runtime thread panic probe"),
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("runtime panic event timeout"),
            Some(RuntimeEvent::ThreadPanicked)
        ));
    }

    #[tokio::test]
    async fn terminate_execution_stops_cpu_bound_module() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request("while (true) {}"),
            60_000,
            event_tx,
            std::sync::Arc::new(OutputAdmission::new(super::MAX_BUFFERED_OUTPUT_BYTES)),
            /*task_failure_handler*/ None,
        )
        .await
        .unwrap();

        let started_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(started_event, RuntimeEvent::Started));

        assert!(runtime_terminate_handle.terminate_execution());

        let result_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::Result { error_text, .. } = result_event else {
            panic!("expected runtime result after termination");
        };
        assert!(error_text.is_some());

        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropping_an_unclaimed_startup_handle_stops_cpu_bound_code() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request("while (true) {}"),
            60_000,
            event_tx,
            std::sync::Arc::new(OutputAdmission::new(super::MAX_BUFFERED_OUTPUT_BYTES)),
            None,
        )
        .await
        .unwrap();
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        assert!(
            startup_tx
                .send(super::StartupIsolateHandle(Some(runtime_terminate_handle)))
                .is_ok()
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap(),
            Some(RuntimeEvent::Started)
        ));

        drop(startup_rx);

        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap();
        assert!(matches!(
            event,
            Some(RuntimeEvent::Result {
                error_text: Some(_),
                ..
            })
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn yield_admission_bounds_a_synchronous_flood() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request("for (let i = 0; i < 100_000; i++) yield_control();"),
            60_000,
            event_tx,
            std::sync::Arc::new(OutputAdmission::new(super::MAX_BUFFERED_OUTPUT_BYTES)),
            None,
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            assert!(matches!(event_rx.recv().await, Some(RuntimeEvent::Started)));
            assert!(matches!(
                event_rx.recv().await,
                Some(RuntimeEvent::YieldRequested)
            ));
            assert!(matches!(
                event_rx.recv().await,
                Some(RuntimeEvent::Result {
                    error_text: None,
                    ..
                })
            ));
            assert!(event_rx.recv().await.is_none());
        })
        .await
        .expect("yield flood must complete without a retained event backlog");
    }

    #[tokio::test]
    async fn runtime_output_admission_bounds_a_synchronous_flood() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let output_admission = std::sync::Arc::new(OutputAdmission::new(128));
        let (_runtime_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(
                r#"for (let i = 0; i < 1_000; i++) text("01234567890123456789012345678901");"#,
            ),
            60_000,
            event_tx,
            std::sync::Arc::clone(&output_admission),
            /*task_failure_handler*/ None,
        )
        .await
        .unwrap();

        let mut admitted_bytes = 0usize;
        let mut content_count = 0usize;
        let mut overflow_count = 0usize;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("runtime event timeout")
                .expect("runtime must report a result");
            match event {
                RuntimeEvent::ContentItem {
                    item,
                    admitted_bytes: item_bytes,
                } => {
                    content_count += 1;
                    admitted_bytes += item_bytes;
                    if matches!(
                        item,
                        codex_code_mode_protocol::FunctionCallOutputContentItem::InputText {
                            ref text
                        } if text == OUTPUT_LIMIT_MESSAGE
                    ) {
                        overflow_count += 1;
                    }
                }
                RuntimeEvent::Result { error_text, output_loss, .. } => {
                    assert_eq!(error_text, None);
                    let loss = output_loss.expect("discarded output must be reported structurally");
                    assert_eq!(loss.discarded_items, (1_000 - (content_count - overflow_count)) as u64);
                    assert_eq!(loss.discarded_bytes_lower_bound, loss.discarded_items * 32);
                    break;
                }
                RuntimeEvent::Started => {}
                event => panic!("unexpected runtime event: {event:?}"),
            }
        }

        assert!(admitted_bytes <= 128);
        assert_eq!(overflow_count, 1);
        assert!(content_count < 1_000);
    }

    #[test]
    fn output_admission_resets_after_marker_only_delivery() {
        let output_admission = OutputAdmission::new(1);
        let oversized_item = codex_code_mode_protocol::FunctionCallOutputContentItem::InputText {
            text: "oversized".to_string(),
        };

        let first_event = output_admission
            .admit(oversized_item.clone())
            .expect("first overflow should emit a marker");
        assert!(matches!(
            first_event,
            RuntimeEvent::ContentItem {
                admitted_bytes: 0,
                ..
            }
        ));

        output_admission.release(0);

        let second_event = output_admission
            .admit(oversized_item)
            .expect("a later overflow should emit its own marker");
        assert!(matches!(
            second_event,
            RuntimeEvent::ContentItem {
                admitted_bytes: 0,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod regression_tests;
