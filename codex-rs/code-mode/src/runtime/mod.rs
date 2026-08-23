mod callbacks;
mod globals;
mod module_loader;
mod timers;
mod value;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::enabled_tool_metadata;
use codex_protocol::ToolName;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

const EXIT_SENTINEL: &str = "__codex_code_mode_exit__";
const RUNTIME_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    NotificationResponse { id: String },
    NotificationError { id: String, error_text: String },
    TimeoutFired { id: u64 },
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    ContentItem(FunctionCallOutputContentItem),
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
    },
    ThreadPanicked,
}

pub(crate) fn spawn_runtime(
    stored_values: HashMap<String, JsonValue>,
    request: ExecuteRequest,
    default_tool_timeout_ms: u64,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<(std_mpsc::Sender<RuntimeCommand>, v8::IsolateHandle), String> {
    ensure_v8_initialized()?;

    let (command_tx, command_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let (isolate_handle_tx, isolate_handle_rx) = std_mpsc::sync_channel(1);
    let startup_cancelled = Arc::new(AtomicBool::new(false));
    let enabled_tools = request
        .enabled_tools
        .iter()
        .map(enabled_tool_metadata)
        .collect::<Vec<_>>();
    let config = RuntimeConfig {
        tool_call_id: request.tool_call_id,
        enabled_tools,
        source: request.source,
        stored_values,
        default_tool_timeout_ms,
    };

    let runtime_startup_cancelled = Arc::clone(&startup_cancelled);
    spawn_supervised_runtime_thread(event_tx.clone(), task_failure_handler, move || {
        run_runtime(
            config,
            event_tx,
            command_rx,
            isolate_handle_tx,
            runtime_command_tx,
            runtime_startup_cancelled,
        );
    });

    let isolate_handle = receive_runtime_startup(
        &isolate_handle_rx,
        startup_cancelled.as_ref(),
        RUNTIME_STARTUP_TIMEOUT,
    )?;
    Ok((command_tx, isolate_handle))
}

fn receive_runtime_startup<T>(
    receiver: &std_mpsc::Receiver<T>,
    startup_cancelled: &AtomicBool,
    timeout: Duration,
) -> Result<T, String> {
    match receiver.recv_timeout(timeout) {
        Ok(value) => Ok(value),
        Err(std_mpsc::RecvTimeoutError::Timeout) => {
            startup_cancelled.store(true, Ordering::Release);
            Err(format!(
                "code mode runtime initialization exceeded its {}ms timeout",
                timeout.as_millis()
            ))
        }
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            startup_cancelled.store(true, Ordering::Release);
            Err("failed to initialize code mode runtime".to_string())
        }
    }
}

fn spawn_supervised_runtime_thread(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
    runtime: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        if catch_unwind(AssertUnwindSafe(runtime)).is_err() {
            if let Some(task_failure_handler) = task_failure_handler {
                task_failure_handler("code-mode V8 runtime thread panicked".to_string());
            }
            let _ = event_tx.send(RuntimeEvent::ThreadPanicked);
        }
    });
}

#[derive(Clone)]
struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Vec<EnabledToolMetadata>,
    source: String,
    stored_values: HashMap<String, JsonValue>,
    default_tool_timeout_ms: u64,
}

pub(super) struct RuntimeState {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_notifications: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    stored_values: HashMap<String, JsonValue>,
    stored_value_writes: HashMap<String, JsonValue>,
    enabled_tools: Vec<EnabledToolMetadata>,
    next_tool_call_id: u64,
    next_notification_id: u64,
    next_timeout_id: u64,
    default_tool_timeout_ms: u64,
    tool_call_id: String,
    timer_scheduler: timers::TimerScheduler,
    exit_requested: bool,
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
    isolate_handle_tx: std_mpsc::SyncSender<v8::IsolateHandle>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    startup_cancelled: Arc<AtomicBool>,
) {
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
    if startup_cancelled.load(Ordering::Acquire) {
        return;
    }
    let isolate_handle = isolate.thread_safe_handle();
    if isolate_handle_tx.send(isolate_handle).is_err() {
        return;
    }
    isolate.set_host_import_module_dynamically_callback(module_loader::dynamic_import_callback);

    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let timer_scheduler = timers::TimerScheduler::new(runtime_command_tx);
    scope.set_slot(RuntimeState {
        event_tx: event_tx.clone(),
        pending_tool_calls: HashMap::new(),
        pending_notifications: HashMap::new(),
        pending_timeouts: HashMap::new(),
        stored_values: config.stored_values,
        stored_value_writes: HashMap::new(),
        enabled_tools: config.enabled_tools,
        next_tool_call_id: 1,
        next_notification_id: 1,
        next_timeout_id: 1,
        default_tool_timeout_ms: config.default_tool_timeout_ms,
        tool_call_id: config.tool_call_id,
        timer_scheduler,
        exit_requested: false,
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_result(&event_tx, HashMap::new(), Some(error_text));
        return;
    }

    let _ = event_tx.send(RuntimeEvent::Started);

    let pending_promise = match module_loader::evaluate_main_module(scope, &config.source) {
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
            send_result(&event_tx, stored_value_writes, error_text);
            return;
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Ok(command) = command_rx.recv() {
        match command {
            RuntimeCommand::Terminate => break,
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
            RuntimeCommand::TimeoutFired { id } => {
                if let Err(runtime_error) = timers::invoke_timeout_callback(scope, id) {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
        }

        scope.perform_microtask_checkpoint();
        match module_loader::completion_state(scope, pending_promise.as_ref()) {
            CompletionState::Completed {
                stored_value_writes,
                error_text,
            } => {
                send_result(&event_tx, stored_value_writes, error_text);
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
    let stored_value_writes = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.stored_value_writes.clone())
        .unwrap_or_default();

    send_result(event_tx, stored_value_writes, error_text);
}

fn send_result(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    stored_value_writes: HashMap<String, JsonValue>,
    error_text: Option<String>,
) {
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        error_text,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::ExecuteRequest;
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
        }
    }

    #[test]
    fn runtime_startup_receive_is_bounded_and_marks_late_initialization_cancelled() {
        let (_startup_tx, startup_rx) = std::sync::mpsc::channel::<()>();
        let startup_cancelled = std::sync::atomic::AtomicBool::new(false);

        let error =
            receive_runtime_startup(&startup_rx, &startup_cancelled, Duration::from_millis(1))
                .expect_err("withheld startup handle should time out");

        assert_eq!(
            error,
            "code mode runtime initialization exceeded its 1ms timeout"
        );
        assert!(startup_cancelled.load(std::sync::atomic::Ordering::Acquire));
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
            "code-mode V8 runtime thread panicked"
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
            /*task_failure_handler*/ None,
        )
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
}
