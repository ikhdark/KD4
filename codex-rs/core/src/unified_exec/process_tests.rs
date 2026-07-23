use super::ProcessEntry;
use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UnifiedExecContext;
use super::UnifiedExecProcessManager;
use super::async_watcher::omitted_output_marker;
use super::async_watcher::resolve_aggregated_output;
use super::async_watcher::start_streaming_output;
use super::head_tail_buffer::HeadTailBuffer;
use super::process::UnifiedExecProcess;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
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
use codex_tools::ToolExecutor;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
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
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
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
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            wake_tx,
        }),
    };

    UnifiedExecProcess::from_exec_server_started(started, None)
        .await
        .expect("remote process should start")
}

async fn store_process_for_test(
    manager: &UnifiedExecProcessManager,
    session: &Arc<Session>,
    turn: &TurnContext,
    process_id: i32,
    process: Arc<UnifiedExecProcess>,
) {
    #[allow(deprecated)]
    let cwd = turn.cwd.clone().into();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process,
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
    process_id: i32,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
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

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
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

#[tokio::test]
async fn output_published_before_streaming_starts_is_retained() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;
    let marker = b"startup-output".to_vec();

    process.publish_output_for_test(marker.clone()).await;
    let mut receiver = process.take_output_receiver();

    assert_eq!(receiver.recv().await.expect("reserved output"), marker);
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
    start_streaming_output(&process, &context, Arc::clone(&transcript));

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

    let output_drained = process.output_drained_notify();
    process.terminate();
    tokio::time::timeout(Duration::from_secs(2), output_drained.notified())
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
    let process_a = Arc::new(remote_process(WriteStatus::Accepted, None).await);
    let process_b = Arc::new(remote_process(WriteStatus::Accepted, None).await);
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
    let process = Arc::new(remote_process(WriteStatus::Accepted, None).await);
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
