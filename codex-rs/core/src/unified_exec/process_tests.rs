use super::NoopSpawnLifecycle;
use super::PendingSpawnRegistration;
use super::ProcessEntry;
use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UnifiedExecContext;
use super::UnifiedExecProcessManager;
use super::async_watcher::omitted_output_marker;
use super::async_watcher::resolve_aggregated_output;
use super::async_watcher::spawn_exit_watcher;
use super::async_watcher::start_streaming_output;
use super::head_tail_buffer::HeadTailBuffer;
use super::process::OutputHandles;
use super::process::UnifiedExecProcess;
use super::process_manager::PendingProcessRegistration;
use super::process_manager::arm_validation_timeout;
use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::WriteStdinHandler;
use crate::tools::registry::CoreToolRuntime;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecOutputStream;
use codex_protocol::protocol::WarningEvent;
use codex_sandboxing::SandboxType;
use codex_tools::ToolExecutor;
use codex_utils_pty::spawn_pipe_process_no_stdin;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct TerminationControl {
    started: Notify,
    allowed: watch::Sender<bool>,
    completed: AtomicBool,
    calls: AtomicUsize,
}

impl TerminationControl {
    fn new() -> Self {
        let (allowed, _allowed_rx) = watch::channel(false);
        Self {
            started: Notify::new(),
            allowed,
            completed: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        }
    }
}

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    termination_control: Option<Arc<TerminationControl>>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(control) = &self.termination_control {
            control.calls.fetch_add(1, Ordering::AcqRel);
            let mut allowed = control.allowed.subscribe();
            control.started.notify_one();
            let _ = allowed.wait_for(|allowed| *allowed).await;
        }
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        if let Some(control) = &self.termination_control {
            control.completed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
) -> Arc<UnifiedExecProcess> {
    remote_process_with_termination_control(write_status, terminate_error, None).await
}

async fn remote_process_with_termination_control(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    termination_control: Option<Arc<TerminationControl>>,
) -> Arc<UnifiedExecProcess> {
    remote_process_with_options(write_status, terminate_error, termination_control, None).await
}

async fn remote_process_with_options(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    termination_control: Option<Arc<TerminationControl>>,
    validation_timeout_ms: Option<u64>,
) -> Arc<UnifiedExecProcess> {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            termination_control,
            wake_tx,
        }),
    };

    UnifiedExecProcess::from_exec_server_started(
        started,
        None,
        validation_timeout_ms,
        &PendingSpawnRegistration::default(),
    )
    .await
    .expect("remote process should start")
}

async fn store_process_for_test(
    manager: &UnifiedExecProcessManager,
    session: &Arc<Session>,
    turn: &TurnContext,
    process_id: u32,
    process: Arc<UnifiedExecProcess>,
) {
    let cwd = turn.cwd().clone().into();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process,
            command_execution_id: Default::default(),
            parent_tool_execution_id: Default::default(),
            call_id: format!("exec-call-{process_id}"),
            process_id,
            cwd,
            initial_exec_command_active: Arc::new(AtomicBool::new(false)),
            hook_command: format!("test-command-{process_id}"),
            tty: true,
            network_approval: None,
            session: Arc::downgrade(session),
            last_used: Instant::now(),
        },
    );
}

fn write_stdin_invocation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: &str,
    process_id: u32,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain("write_stdin"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "session_id": process_id,
                "chars": "",
                "yield_time_ms": 60_000,
            })
            .to_string(),
        },
    }
}

async fn wait_for_process_clones(process: &Arc<UnifiedExecProcess>, minimum: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while Arc::strong_count(process) < minimum {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("write_stdin calls should clone the process before waiting on its interaction lock");
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(WriteStatus::UnknownProcess, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(WriteStatus::StdinClosed, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process
        .fail_and_terminate("network denied".to_string())
        .await
        .expect("first termination succeeds");
    process
        .fail_and_terminate("second failure".to_string())
        .await
        .expect("repeated termination remains successful");

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn validation_timeout_terminates_live_process() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    arm_validation_timeout(Arc::clone(&process), Some(25));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(25)).await;
    tokio::task::yield_now().await;

    assert!(process.timed_out());
    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("validation timed out after 25 ms".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn validation_timeout_starts_before_early_exit_grace_finishes() {
    let process = remote_process_with_options(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        /*termination_control*/ None,
        Some(25),
    )
    .await;

    assert!(process.timed_out());
    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("validation timed out after 25 ms".to_string())
    );
}

#[tokio::test]
async fn unified_exec_termination_failure_retains_process_owner() {
    let manager = UnifiedExecProcessManager::default();
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;
    let process_id = 1_001;
    store_process_for_test(&manager, &session, &turn, process_id, Arc::clone(&process)).await;

    let error = manager
        .fail_process_with_message(process_id, &process, "network denied".to_string())
        .await;

    assert!(matches!(error, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .get(&process_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process)),
        "the manager must retain the process owner when termination is unconfirmed"
    );
}

#[tokio::test(start_paused = true)]
async fn validation_timeout_termination_failure_terminalizes_once_and_retains_process_owner() {
    let manager = UnifiedExecProcessManager::default();
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;
    let process_id = 1_002;
    let call_id = "exec-call-validation-timeout".to_string();
    let command = vec!["cargo".to_string(), "test".to_string()];
    let context = UnifiedExecContext::new(Arc::clone(&session), Arc::clone(&turn), call_id.clone());
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    start_streaming_output(&process, &context, Arc::clone(&transcript))
        .expect("streaming output should start");

    let command_execution_id = session.services.command_execution.allocate_execution_id();
    let parent_tool_execution_id = codex_protocol::protocol::ToolExecutionId::default();
    let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "test",
        "test-cwd",
        &command,
    );
    session
        .services
        .command_execution
        .begin_attempt(&attempt_key, false)
        .await
        .expect("begin validation attempt");
    let started_at = Instant::now();
    let validation_launch = crate::validation_admission::ValidationLaunchPlan {
        classification: crate::validation_admission::classify_validation(
            &crate::tools::handlers::command_shape::CommandInvocation::Argv {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
            },
        ),
        authorization_revision: 0,
        explicitly_tagged: true,
        structured_route: Some(codex_protocol::plan_tool::ValidationRoute {
            leaves: vec![codex_protocol::plan_tool::ValidationRouteLeaf {
                argv: command.clone(),
                covered_paths: vec!["core/src/unified_exec/process.rs".to_string()],
                timeout_ms: 25,
            }],
            ordering: Default::default(),
        }),
        bound_plan_step: None,
        bound_work_unit: None,
        validation_call_id: Some("validation-timeout-call".to_string()),
        turn_timing_state: Some(Arc::clone(&turn.turn_timing_state)),
        focused_validation_token: None,
    };
    session
        .services
        .command_execution
        .track_running_process_with_execution_id(
            command_execution_id,
            parent_tool_execution_id.clone(),
            process_id,
            attempt_key,
            RawOutputArtifact::unavailable("validation timeout fixture"),
            Some(validation_launch),
            started_at,
        )
        .await
        .expect("track validation process");

    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id,
            parent_tool_execution_id: parent_tool_execution_id.clone(),
            call_id: call_id.clone(),
            process_id,
            cwd: turn.cwd().clone().into(),
            initial_exec_command_active: Arc::new(AtomicBool::new(false)),
            hook_command: command.join(" "),
            tty: true,
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: started_at,
        },
    );
    spawn_exit_watcher(
        Arc::clone(&process),
        Arc::clone(&session),
        Arc::clone(&turn),
        call_id.clone(),
        command.clone(),
        turn.cwd().clone().into(),
        "test".to_string(),
        process_id,
        command_execution_id,
        parent_tool_execution_id,
        transcript,
        started_at,
        /*tracker*/ None,
        /*known_delta*/ None,
        /*known_delta_executor_started_at*/ None,
        /*tool_dispatch_timing*/ None,
    );

    arm_validation_timeout(Arc::clone(&process), Some(25));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(25)).await;

    let completed_item = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event
                .recv()
                .await
                .expect("event channel should remain open");
            if let codex_protocol::protocol::EventMsg::ItemCompleted(event) = event.msg
                && let codex_protocol::items::TurnItem::CommandExecution(item) = event.item
                && item.id == call_id
            {
                break item;
            }
        }
    })
    .await
    .expect("timed-out validation should publish a terminal command item");

    assert_eq!(
        completed_item.status,
        codex_protocol::items::CommandExecutionStatus::Failed
    );
    assert_eq!(completed_item.process_id.as_deref(), Some("1002"));
    assert!(
        completed_item
            .aggregated_output
            .as_deref()
            .is_some_and(|output| output.contains("validation timed out after 25 ms"))
    );
    assert!(process.timed_out());
    assert!(!process.has_exited());
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .get(&process_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process)),
        "an unconfirmed remote process must remain owned after validation terminalization"
    );
    assert!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_some(),
        "process bookkeeping must remain live until an actual exit"
    );
    assert!(
        session
            .services
            .command_execution
            .complete_timed_out_running_validation(process_id)
            .await
            .is_none(),
        "the timeout path must consume the validation terminal exactly once"
    );
    assert_eq!(
        turn.turn_timing_state
            .complete_snapshot()
            .protocol_timing()
            .counters
            .executed_validation_count,
        1
    );

    process.signal_exit_for_test(Some(0));
    process.finish_termination();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if session
                .services
                .command_execution
                .process_execution_identity(process_id)
                .await
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual exit should release command bookkeeping");

    let mut duplicate_terminal_items = 0;
    while let Ok(event) = rx_event.try_recv() {
        if let codex_protocol::protocol::EventMsg::ItemCompleted(event) = event.msg
            && let codex_protocol::items::TurnItem::CommandExecution(item) = event.item
            && item.id == call_id
        {
            duplicate_terminal_items += 1;
        }
    }
    assert_eq!(duplicate_terminal_items, 0);
    assert_eq!(
        turn.turn_timing_state
            .complete_snapshot()
            .protocol_timing()
            .counters
            .executed_validation_count,
        1
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_updates_state_on_success_only() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
}

#[tokio::test(start_paused = true)]
async fn remote_terminate_confirmed_has_a_total_deadline_and_remains_retryable() {
    let termination_control = Arc::new(TerminationControl::new());
    let process = remote_process_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        Some(Arc::clone(&termination_control)),
    )
    .await;

    let error = process
        .terminate_confirmed()
        .await
        .expect_err("unconfirmed termination should time out");

    assert!(
        error
            .to_string()
            .contains("timed out confirming process termination")
    );
    assert!(!process.has_exited());
    assert_eq!(termination_control.calls.load(Ordering::Acquire), 1);

    termination_control.allowed.send_replace(true);
    process
        .terminate_confirmed()
        .await
        .expect("a later termination attempt should remain possible");
    assert!(process.has_exited());
    assert_eq!(termination_control.calls.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn dropping_confirmed_remote_process_does_not_terminate_twice() {
    let termination_control = Arc::new(TerminationControl::new());
    termination_control.allowed.send_replace(true);
    let process = remote_process_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        Some(Arc::clone(&termination_control)),
    )
    .await;

    process
        .terminate_confirmed()
        .await
        .expect("remote termination should be confirmed");
    assert_eq!(termination_control.calls.load(Ordering::Acquire), 1);

    drop(process);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while termination_control.calls.load(Ordering::Acquire) == 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "dropping an exited process must not issue another terminate RPC"
    );
    assert_eq!(termination_control.calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn spawned_process_is_retained_when_constructor_future_is_cancelled() {
    let termination_control = Arc::new(TerminationControl::new());
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "cancelled-constructor".to_string().into(),
            write_response: WriteResponse {
                status: WriteStatus::Accepted,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error: None,
            termination_control: Some(Arc::clone(&termination_control)),
            wake_tx,
        }),
    };
    let pending_spawns = PendingSpawnRegistration::default();
    let mut constructor = Box::pin(UnifiedExecProcess::from_exec_server_started(
        started,
        None,
        None,
        &pending_spawns,
    ));

    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut constructor)
            .await
            .is_err(),
        "constructor should still be inside the early-exit grace period"
    );
    drop(constructor);
    let retained = pending_spawns.snapshot();
    assert_eq!(retained.len(), 1);

    let process = Arc::clone(&retained[0]);
    let terminate_task = tokio::spawn(async move { process.terminate_confirmed().await });
    tokio::time::timeout(
        Duration::from_secs(1),
        termination_control.started.notified(),
    )
    .await
    .expect("confirmed termination starts");
    assert!(!termination_control.completed.load(Ordering::Acquire));
    termination_control
        .allowed
        .send(true)
        .expect("termination waiter remains subscribed");
    terminate_task
        .await
        .expect("termination task joins")
        .expect("termination succeeds");
    assert!(termination_control.completed.load(Ordering::Acquire));
    pending_spawns.clear();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_startup_keeps_store_and_ledger_until_termination_is_confirmed() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let manager = &session.services.unified_exec_manager;
    let process_id = 4_242;
    let termination_control = Arc::new(TerminationControl::new());
    let process = remote_process_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        Some(Arc::clone(&termination_control)),
    )
    .await;
    let active = Arc::new(AtomicBool::new(true));
    let cwd = turn.cwd().clone().into();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id: Default::default(),
            parent_tool_execution_id: Default::default(),
            call_id: "cancelled-startup".to_string(),
            process_id,
            cwd,
            initial_exec_command_active: Arc::clone(&active),
            hook_command: "blocking-test".to_string(),
            tty: false,
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
        },
    );
    let attempt_key = crate::tools::command_execution::CommandAttemptKey::new(
        "exec_command",
        "test",
        "test-cwd",
        &["blocking-test".to_string()],
    );
    session
        .services
        .command_execution
        .track_running_process(
            process_id,
            attempt_key.clone(),
            crate::tools::command_output_artifact::RawOutputArtifact::unavailable(
                "cancelled startup fixture",
            ),
        )
        .await
        .expect("track running process");
    let context =
        UnifiedExecContext::new(Arc::clone(&session), Arc::clone(&turn), "call".to_string());
    let mut registration = PendingProcessRegistration::new(
        Arc::clone(&manager.process_store),
        &context,
        attempt_key,
        process_id,
    );
    registration.attach_process(Arc::clone(&process), None);
    registration.set_initial_exec_command_active(Arc::clone(&active));

    drop(registration);

    assert!(!active.load(Ordering::Acquire));
    tokio::time::timeout(
        Duration::from_secs(1),
        termination_control.started.notified(),
    )
    .await
    .expect("termination starts");
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );
    assert!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_some()
    );

    termination_control
        .allowed
        .send(true)
        .expect("termination waiters remain subscribed");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !manager
                .process_store
                .lock()
                .await
                .processes
                .contains_key(&process_id)
                && session
                    .services
                    .command_execution
                    .running_process(process_id)
                    .await
                    .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleanup finishes after termination confirmation");
    assert!(termination_control.completed.load(Ordering::Acquire));
}

#[tokio::test]
async fn terminate_all_processes_confirms_remote_termination_for_failed_process() {
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let termination_control = Arc::new(TerminationControl::new());
    let process = remote_process_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        Some(Arc::clone(&termination_control)),
    )
    .await;
    store_process_for_test(&manager, &session, &turn, 1000, Arc::clone(&process)).await;

    let process_for_failure = Arc::clone(&process);
    let failure_task = tokio::spawn(async move {
        process_for_failure
            .fail_and_terminate("test failure".to_string())
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        termination_control.started.notified(),
    )
    .await
    .expect("detached remote termination should start");
    assert!(!process.has_exited());

    let manager_for_shutdown = Arc::clone(&manager);
    let shutdown_task = tokio::spawn(async move {
        manager_for_shutdown.terminate_all_processes().await;
    });

    tokio::task::yield_now().await;
    assert!(!shutdown_task.is_finished());
    assert!(!termination_control.completed.load(Ordering::Acquire));

    termination_control.allowed.send_replace(true);
    failure_task
        .await
        .expect("failure task joins")
        .expect("failure termination succeeds");
    tokio::time::timeout(Duration::from_secs(2), shutdown_task)
        .await
        .expect("shutdown should finish after remote termination")
        .expect("shutdown task should succeed");

    assert!(termination_control.completed.load(Ordering::Acquire));
    assert!(process.has_exited());
    assert!(manager.process_store.lock().await.processes.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_cleanup_terminates_only_unpublished_call_owners_before_removal() {
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let termination_control = Arc::new(TerminationControl::new());
    let unpublished = remote_process_with_termination_control(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        Some(Arc::clone(&termination_control)),
    )
    .await;
    let published = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    store_process_for_test(&manager, &session, &turn, 1000, Arc::clone(&unpublished)).await;
    store_process_for_test(&manager, &session, &turn, 1001, Arc::clone(&published)).await;

    let manager_for_cleanup = Arc::clone(&manager);
    let cleanup = tokio::spawn(async move {
        manager_for_cleanup
            .terminate_unpublished_processes_for_call_ids(&["exec-call-1000".to_string()])
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        termination_control.started.notified(),
    )
    .await
    .expect("abort cleanup should request confirmed termination");
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&1000),
        "ownership must remain registered until termination is confirmed"
    );

    termination_control.allowed.send_replace(true);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), cleanup)
            .await
            .expect("abort cleanup should finish")
            .expect("abort cleanup task should join")
            .expect("abort cleanup should succeed"),
        1
    );
    assert!(termination_control.completed.load(Ordering::Acquire));
    assert!(unpublished.has_exited());
    assert!(!published.has_exited());
    let store = manager.process_store.lock().await;
    assert!(!store.processes.contains_key(&1000));
    assert!(store.processes.contains_key(&1001));
}

#[tokio::test]
async fn abort_cleanup_retains_owner_when_termination_is_unconfirmed() {
    let manager = UnifiedExecProcessManager::default();
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let process = remote_process(
        WriteStatus::Accepted,
        Some("remote refused termination".to_string()),
    )
    .await;
    store_process_for_test(&manager, &session, &turn, 1000, Arc::clone(&process)).await;

    let error = manager
        .terminate_unpublished_processes_for_call_ids(&["exec-call-1000".to_string()])
        .await
        .expect_err("unconfirmed termination must prevent owner removal");

    assert!(error.contains("did not confirm termination"));
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&1000)
    );
}

#[tokio::test]
async fn output_published_before_streaming_starts_is_retained() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let marker = b"startup-output".to_vec();

    process.publish_output_for_test(marker.clone()).await;
    let mut receiver = process
        .take_output_receiver()
        .expect("reserved output receiver");

    assert_eq!(
        receiver.recv().await.expect("reserved output").bytes,
        marker
    );
}

#[tokio::test]
async fn startup_output_reaches_initial_and_final_transcripts_once() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let marker = b"phase90-startup-output".to_vec();
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let (session, turn) = make_session_and_context().await;
    let context = UnifiedExecContext::new(
        Arc::new(session),
        Arc::new(turn),
        "phase90-startup-call".to_string(),
    );

    process.publish_output_for_test(marker.clone()).await;
    start_streaming_output(&process, &context, Arc::clone(&transcript))
        .expect("start output streaming");

    let handles = process.output_handles();
    let initial = UnifiedExecProcessManager::collect_output_until_deadline(
        &handles.output_buffer,
        &handles.output_notify,
        &handles.output_closed,
        &handles.output_closed_notify,
        &handles.cancellation_token,
        /*pause_state*/ None,
        Instant::now() + Duration::from_millis(10),
    )
    .await;

    let output_drained = process.output_drained_token();
    process.terminate();
    tokio::time::timeout(Duration::from_secs(2), output_drained.cancelled())
        .await
        .expect("streaming output should drain after process termination");
    let final_output = resolve_aggregated_output(&transcript, String::new()).await;
    let marker = String::from_utf8(marker).expect("marker is UTF-8");

    assert_eq!(
        String::from_utf8(initial).expect("initial output is UTF-8"),
        marker
    );
    assert_eq!(final_output.matches(&marker).count(), 1);
}

#[tokio::test]
async fn closure_before_streaming_subscription_drains_lagged_split_utf8_output() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    for _ in 0..64 {
        process.publish_output_for_test(b"x".to_vec()).await;
    }
    process.publish_output_for_test(vec![0xc3]).await;
    process.publish_output_for_test(vec![0xa9]).await;
    process.terminate();

    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let (session, turn) = make_session_and_context().await;
    let context = UnifiedExecContext::new(
        Arc::new(session),
        Arc::new(turn),
        "closure-before-subscription".to_string(),
    );
    let output_drained = process.output_drained_token();

    start_streaming_output(&process, &context, Arc::clone(&transcript))
        .expect("start output streaming");
    tokio::time::timeout(Duration::from_secs(2), output_drained.cancelled())
        .await
        .expect("pre-observed closure should drain without hanging");

    let final_output = resolve_aggregated_output(&transcript, String::new()).await;
    assert!(final_output.contains("streaming receiver lagged by 2 chunk(s)"));
    assert!(
        final_output.contains('é'),
        "final output lost the trailing UTF-8 character: {final_output:?}"
    );
}

#[tokio::test]
async fn saturated_live_event_queue_does_not_block_output_drain_or_completion_snapshot() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let expected = format!("{}é", "x".repeat(64));
    for _ in 0..64 {
        process.publish_output_for_test(b"x".to_vec()).await;
    }
    process.publish_output_for_test(vec![0xc3]).await;
    process.publish_output_for_test(vec![0xa9]).await;
    process.terminate();

    let (session, turn, _rx_event) = make_session_and_context_with_rx().await;
    let mut queued_events = 0;
    loop {
        let accepted = session.try_send_live_event_accepted(Event {
            id: turn.sub_id.clone(),
            msg: EventMsg::Warning(WarningEvent {
                message: format!("fill live event queue {queued_events}"),
            }),
        });
        if !accepted {
            break;
        }
        queued_events += 1;
    }
    assert!(
        queued_events > 0,
        "test must saturate a bounded event queue"
    );

    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "saturated-live-event-queue".to_string(),
    );
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let output_drained = process.output_drained_token();
    start_streaming_output(&process, &context, transcript).expect("start output streaming");

    tokio::time::timeout(Duration::from_secs(2), output_drained.cancelled())
        .await
        .expect("a full live event queue must not block output drain");
    let snapshot = process.snapshot_completion_output().await;
    assert_eq!(
        String::from_utf8(snapshot.aggregated_output).expect("completion output is UTF-8"),
        expected
    );
    assert!(snapshot.aggregated_output_is_exact);
}

#[tokio::test]
async fn closure_after_streaming_subscription_wakes_all_drain_waiters() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let transcript = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let (session, turn) = make_session_and_context().await;
    let context = UnifiedExecContext::new(
        Arc::new(session),
        Arc::new(turn),
        "closure-after-subscription".to_string(),
    );
    let output_drained = process.output_drained_token();

    start_streaming_output(&process, &context, Arc::clone(&transcript))
        .expect("start output streaming");
    tokio::task::yield_now().await;
    process.publish_output_for_test(b"tail:".to_vec()).await;
    process.publish_output_for_test(vec![0xc3]).await;
    process.publish_output_for_test(vec![0xa9]).await;
    let initial_response_waiter = output_drained.clone();
    let exit_finalizer_waiter = output_drained.clone();
    process.terminate();

    tokio::time::timeout(Duration::from_secs(2), async move {
        tokio::join!(
            initial_response_waiter.cancelled(),
            exit_finalizer_waiter.cancelled()
        );
    })
    .await
    .expect("all output-drain waiters should finish without hanging");
    let final_output = resolve_aggregated_output(&transcript, String::new()).await;
    assert_eq!(final_output, "tail:é");
    tokio::time::timeout(Duration::from_millis(150), output_drained.cancelled())
        .await
        .expect("output drain should remain observable to future waiters");
}

#[tokio::test]
async fn local_output_artifact_is_flushed_and_unlocked_before_output_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = Arc::new(Mutex::new(
        create_raw_output_artifact(temp.path(), "completion-barrier", b"").await,
    ));
    let output_buffer = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let output_handles = OutputHandles {
        output_buffer: Arc::clone(&output_buffer),
        completion_output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        stdout_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        stderr_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        output_notify,
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::clone(&output_closed_notify),
        cancellation_token: CancellationToken::new(),
    };
    let (output_tx, mut output_rx) = tokio::sync::broadcast::channel(8);
    let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel(1);
    let (stderr_tx, stderr_rx) = tokio::sync::mpsc::channel(1);
    let closed = output_closed_notify.notified();
    tokio::pin!(closed);
    closed.as_mut().enable();

    let output_task = UnifiedExecProcess::spawn_local_output_task(
        stdout_rx,
        stderr_rx,
        output_handles,
        output_tx,
        Some(Arc::clone(&artifact)),
    );
    stdout_tx
        .send(b"artifact-tail".to_vec())
        .await
        .expect("stdout remains open");
    let stdout = output_rx.recv().await.expect("stdout broadcast");
    assert_eq!(stdout.stream, ExecOutputStream::Stdout);
    assert_eq!(stdout.bytes, b"artifact-tail");
    stderr_tx
        .send(b"error-tail".to_vec())
        .await
        .expect("stderr remains open");
    let stderr = output_rx.recv().await.expect("stderr broadcast");
    assert_eq!(stderr.stream, ExecOutputStream::Stderr);
    assert_eq!(stderr.bytes, b"error-tail");
    drop(stdout_tx);
    drop(stderr_tx);

    tokio::time::timeout(Duration::from_secs(2), &mut closed)
        .await
        .expect("local output should close");
    assert!(output_closed.load(Ordering::Acquire));
    let (path, handle) = match &*artifact.lock().await {
        RawOutputArtifact::Stored { path, handle, .. } => (path.clone(), Arc::clone(handle)),
        RawOutputArtifact::Pending { .. } => panic!("artifact remained pending"),
        RawOutputArtifact::Failed { message, .. } => panic!("artifact failed: {message}"),
    };
    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("read finalized artifact"),
        b"artifact-tailerror-tail"
    );
    handle.try_lock().expect("artifact should be unlocked");
    handle.unlock().expect("release test artifact lock");
    output_task.await.expect("local output task should finish");
}

#[tokio::test]
async fn terminating_local_process_finalizes_pending_raw_output_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (program, args) = (
        "cmd.exe",
        vec![
            "/D".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            "ping -n 30 127.0.0.1 >nul".to_string(),
        ],
    );
    let spawned = spawn_pipe_process_no_stdin(program, &args, temp.path(), &HashMap::new(), &None)
        .await
        .expect("local fixture process should spawn");
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        SandboxType::None,
        Box::new(NoopSpawnLifecycle),
        Some(RawOutputArtifact::pending(
            temp.path(),
            "termination-finalization",
        )),
        None,
        &PendingSpawnRegistration::default(),
    )
    .await
    .expect("local fixture process should register");
    let output_handles = process.output_handles();
    let output_closed = output_handles.output_closed_notify.notified();
    tokio::pin!(output_closed);
    output_closed.as_mut().enable();

    process.terminate();
    if !output_handles.output_closed.load(Ordering::Acquire) {
        tokio::time::timeout(Duration::from_secs(2), &mut output_closed)
            .await
            .expect("local output task should finalize after termination");
    }

    assert!(
        process.raw_output_artifact().await.is_some(),
        "local termination must not leave the raw-output artifact pending"
    );
}

#[tokio::test]
async fn local_process_exited_before_registration_closes_output_before_denial_check() {
    let temp = tempfile::tempdir().expect("tempdir");
    let args = [
        "/D".to_string(),
        "/S".to_string(),
        "/C".to_string(),
        "echo early-exit-output".to_string(),
    ];
    let spawned =
        spawn_pipe_process_no_stdin("cmd.exe", &args, temp.path(), &HashMap::new(), &None)
            .await
            .expect("quick local fixture process should spawn");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let process = tokio::time::timeout(
        Duration::from_secs(1),
        UnifiedExecProcess::from_spawned(
            spawned,
            SandboxType::None,
            Box::new(NoopSpawnLifecycle),
            None,
            None,
            &PendingSpawnRegistration::default(),
        ),
    )
    .await
    .expect("an exited process must not wait for the I/O-drain timeout")
    .expect("quick local fixture process should register");

    assert!(process.has_exited());
    assert!(
        String::from_utf8_lossy(&process.snapshot_output().await).contains("early-exit-output"),
        "sandbox-denial inspection must see output drained before registration completes"
    );
}

#[tokio::test]
async fn sandbox_denial_snapshot_separates_capacity_omission_seam() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let head_budget = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 2;
    let tail_budget = UNIFIED_EXEC_OUTPUT_MAX_BYTES - head_budget;
    let mut output = vec![b'a'; head_budget - 4];
    output.extend_from_slice(b"pass---word");
    output.extend(std::iter::repeat_n(b'b', tail_budget - 4));
    process
        .output_handles()
        .output_buffer
        .lock()
        .await
        .push_chunk(output);

    let rendered = process.snapshot_output().await;
    let marker = omitted_output_marker(3);

    assert_eq!(
        rendered
            .windows(marker.len())
            .filter(|window| *window == marker.as_slice())
            .count(),
        1
    );
    assert!(
        !rendered
            .windows(b"password".len())
            .any(|window| window == b"password")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_process_polls_do_not_share_an_interaction_lock() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let manager = &session.services.unified_exec_manager;
    let process_a = remote_process(WriteStatus::Accepted, None).await;
    let process_b = remote_process(WriteStatus::Accepted, None).await;
    store_process_for_test(manager, &session, &turn, 1001, Arc::clone(&process_a)).await;
    store_process_for_test(manager, &session, &turn, 1002, Arc::clone(&process_b)).await;

    let interaction_guard = process_a.interaction_lock().lock_owned().await;
    let invocation_a =
        write_stdin_invocation(Arc::clone(&session), Arc::clone(&turn), "poll-a", 1001);
    let poll_a = tokio::spawn(async move { WriteStdinHandler.handle(invocation_a).await });
    wait_for_process_clones(&process_a, 3).await;

    process_b
        .terminate_confirmed()
        .await
        .expect("process B should report confirmed completion");
    let invocation_b =
        write_stdin_invocation(Arc::clone(&session), Arc::clone(&turn), "poll-b", 1002);
    let output_b = tokio::time::timeout(
        Duration::from_secs(2),
        WriteStdinHandler.handle(invocation_b.clone()),
    )
    .await
    .expect("process B should complete while process A remains locked")
    .expect("process B poll should succeed");
    assert_eq!(
        output_b.code_mode_result(&invocation_b.payload)["session_id"],
        serde_json::Value::Null
    );
    assert!(WriteStdinHandler.supports_parallel_tool_calls());

    process_a
        .terminate_confirmed()
        .await
        .expect("process A should report confirmed completion");
    drop(interaction_guard);
    tokio::time::timeout(Duration::from_secs(2), poll_a)
        .await
        .expect("process A poll should finish after its lock is released")
        .expect("process A poll task should not panic")
        .expect("process A poll should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_completed_process_polls_emit_one_completion_and_post_hook() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let manager = &session.services.unified_exec_manager;
    let process = remote_process(WriteStatus::Accepted, None).await;
    store_process_for_test(manager, &session, &turn, 1003, Arc::clone(&process)).await;

    let interaction_guard = process.interaction_lock().lock_owned().await;
    let invocation_a =
        write_stdin_invocation(Arc::clone(&session), Arc::clone(&turn), "poll-a", 1003);
    let invocation_b =
        write_stdin_invocation(Arc::clone(&session), Arc::clone(&turn), "poll-b", 1003);
    let poll_a_invocation = invocation_a.clone();
    let poll_b_invocation = invocation_b.clone();
    let poll_a = tokio::spawn(async move { WriteStdinHandler.handle(poll_a_invocation).await });
    let poll_b = tokio::spawn(async move { WriteStdinHandler.handle(poll_b_invocation).await });
    wait_for_process_clones(&process, 4).await;

    process
        .terminate_confirmed()
        .await
        .expect("process should report confirmed completion");
    drop(interaction_guard);
    let (result_a, result_b) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(poll_a, poll_b)
    })
    .await
    .expect("both completed-process polls should finish");
    let results = [
        (invocation_a, result_a.expect("poll A should not panic")),
        (invocation_b, result_b.expect("poll B should not panic")),
    ];
    let mut completions = 0;
    let mut post_hooks = 0;
    let mut unknown_process_errors = 0;

    for (invocation, result) in results {
        match result {
            Ok(output) => {
                completions += 1;
                assert_eq!(
                    output.code_mode_result(&invocation.payload)["session_id"],
                    serde_json::Value::Null
                );
                if WriteStdinHandler
                    .post_tool_use_payload(&invocation, output.as_ref())
                    .is_some()
                {
                    post_hooks += 1;
                }
            }
            Err(FunctionCallError::RespondToModel(message)) => {
                assert!(message.to_ascii_lowercase().contains("unknown process"));
                unknown_process_errors += 1;
            }
            Err(other) => panic!("unexpected write_stdin error: {other:?}"),
        }
    }

    assert_eq!(completions, 1);
    assert_eq!(post_hooks, 1);
    assert_eq!(unknown_process_errors, 1);
}
