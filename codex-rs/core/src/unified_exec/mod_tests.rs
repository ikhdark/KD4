use super::head_tail_buffer::HeadTailBuffer;
use super::*;
use crate::codex_thread::BackgroundTerminalInfo;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecRequest;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::unified_exec::WriteStdinRequest;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;

const TEST_MAX_OUTPUT_TOKENS: usize = 10_000;

async fn test_session_and_turn() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    (Arc::new(session), Arc::new(turn))
}

async fn exec_command(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    exec_command_with_tty(
        session,
        turn,
        cmd,
        yield_time_ms,
        workdir,
        /*tty*/ true,
    )
    .await
}

fn shell_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn test_exec_request(
    turn: &TurnContext,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    env: HashMap<String, String>,
) -> ExecRequest {
    let windows_sandbox_private_desktop = false;
    let permission_profile = turn.permission_profile();
    let network = None;
    let arg0 = None;
    ExecRequest::new(
        command,
        turn.config.codex_home.clone(),
        cwd,
        env,
        network,
        /*network_environment_id*/ None,
        ExecExpiration::DefaultTimeout,
        ExecCapturePolicy::ShellTool,
        SandboxType::None,
        turn.config.effective_workspace_roots(),
        turn.windows_sandbox_level,
        windows_sandbox_private_desktop,
        permission_profile,
        arg0,
    )
}

async fn exec_command_with_tty(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    cmd: &str,
    yield_time_ms: u64,
    workdir: Option<PathBuf>,
    tty: bool,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    let manager = &session.services.unified_exec_manager;
    let reservation = manager.reserve_process_id().await;
    let process_id = reservation.process_id();
    let cwd = workdir
        .as_ref()
        .map_or_else(|| turn.cwd().clone(), |workdir| turn.cwd().join(workdir));
    let command = if cmd == "powershell.exe -NoExit" {
        vec![
            "powershell.exe".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-NoExit".to_string(),
        ]
    } else {
        vec![
            "powershell.exe".to_string(),
            "-NoLogo".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            cmd.to_string(),
        ]
    };
    let environment = turn
        .environments
        .primary()
        .expect("turn environment")
        .clone();
    let request = ExecCommandRequest {
        validation: None,
        attempt_key: crate::tools::command_execution::CommandAttemptKey::new(
            "exec_command",
            &environment.environment_id,
            cwd.as_path().to_string_lossy().as_ref(),
            &command,
        ),
        command_for_safety: command.clone(),
        command,
        raw_output_artifact: crate::tools::command_output_artifact::RawOutputArtifact::unavailable(
            "test fixture",
        ),
        shell_type: crate::shell::ShellType::PowerShell,
        shell_wrapper_is_owned: true,
        hook_command: cmd.to_string(),
        process_id,
        yield_time_ms,
        max_output_tokens: None,
        cwd: cwd.clone().into(),
        normalization_cwd: None,
        sandbox_cwd: cwd.into(),
        turn_environment: environment,
        network: None,
        tty,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_uri: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
        validation_launch: None,
        known_delta: None,
    };
    let context =
        UnifiedExecContext::new(Arc::clone(session), Arc::clone(turn), "call".to_string());
    manager
        .exec_command(
            request,
            reservation,
            &context,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
}

struct BlockingTerminateExecProcess {
    process_id: ProcessId,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
    wake_tx: watch::Sender<u64>,
}

impl BlockingTerminateExecProcess {
    async fn read(&self) -> Result<ReadResponse, codex_exec_server::ExecServerError> {
        Ok(ReadResponse {
            chunks: Vec::new(),
            next_seq: 1,
            exited: false,
            exit_code: None,
            closed: false,
            failure: None,
            sandbox_denied: false,
        })
    }

    async fn write(&self) -> Result<WriteResponse, codex_exec_server::ExecServerError> {
        Ok(WriteResponse {
            status: WriteStatus::Accepted,
        })
    }

    async fn terminate(&self) -> Result<(), codex_exec_server::ExecServerError> {
        let _ = self.terminate_started.send(true);
        self.allow_terminate.notified().await;
        Ok(())
    }
}

impl ExecProcess for BlockingTerminateExecProcess {
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
        Box::pin(BlockingTerminateExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(BlockingTerminateExecProcess::write(self))
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(BlockingTerminateExecProcess::terminate(self))
    }
}

async fn blocking_terminate_unified_process(
    process_id: u32,
    terminate_started: watch::Sender<bool>,
    allow_terminate: Arc<Notify>,
) -> anyhow::Result<Arc<UnifiedExecProcess>> {
    let (wake_tx, _wake_rx) = watch::channel(0);
    Ok(UnifiedExecProcess::from_exec_server_started(
        StartedExecProcess {
            process: Arc::new(BlockingTerminateExecProcess {
                process_id: process_id.to_string().into(),
                terminate_started,
                allow_terminate,
                wake_tx,
            }),
        },
        None,
        &PendingSpawnRegistration::default(),
    )
    .await?)
}

async fn write_stdin(
    session: &Arc<Session>,
    process_id: u32,
    input: &str,
    yield_time_ms: u64,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    write_stdin_within(session, process_id, input, yield_time_ms, None).await
}

async fn write_stdin_within(
    session: &Arc<Session>,
    process_id: u32,
    input: &str,
    yield_time_ms: u64,
    nested_deadline: Option<std::time::Instant>,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    session
        .services
        .unified_exec_manager
        .write_stdin(WriteStdinRequest {
            process_id,
            input,
            yield_time_ms,
            max_output_tokens: None,
            truncation_policy: TruncationPolicy::Tokens(10_000),
            nested_deadline,
        })
        .await
}

/// Where a poll bounded by `budget` must land.
///
/// The caller states the budget on the standard clock and the handler enforces
/// it on tokio's, so the two differ by however long the fixture took to set up.
/// The window is far tighter than any plausible wrong answer: ignoring the
/// budget entirely yields the configured background maximum, and dropping the
/// return margin yields the full budget.
fn nested_budget_window(budget: Duration) -> std::ops::Range<Duration> {
    const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_millis(500);
    let expected = budget - super::process_manager::NESTED_POLL_MARGIN;
    expected..(expected + CLOCK_SKEW_TOLERANCE)
}

/// Registers a live interactive process the polling tests can observe.
async fn register_pollable_process(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    call_id: &str,
    validation_launch: bool,
) -> anyhow::Result<(u32, Arc<UnifiedExecProcess>, Arc<Notify>)> {
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, _terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id: Default::default(),
            search_exit_one_is_no_match: false,
            parent_tool_execution_id: Default::default(),
            call_id: call_id.to_string(),
            process_id,
            cwd: turn.cwd().clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: call_id.to_string(),
            tty: !validation_launch,
            validation_launch,
            network_approval: None,
            session: Arc::downgrade(session),
            last_used: Instant::now(),
        },
    );
    Ok((process_id, process, allow_terminate))
}

/// A quiet poll that is allowed to wait the whole background budget must return
/// a live process handle *before* the runtime wrapper's hard deadline, not be
/// cancelled at it. Equal values made a full-length wait a guaranteed failure.
#[tokio::test(start_paused = true)]
async fn a_full_length_quiet_poll_yields_inside_the_nested_budget() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "nested-budget", /*validation*/ false).await?;

    let nested_deadline = std::time::Instant::now() + Duration::from_secs(60);
    let started_at = Instant::now();
    let output = write_stdin_within(
        &session,
        process_id,
        "",
        /*yield_time_ms*/ 60_000,
        Some(nested_deadline),
    )
    .await?;

    // The budget is measured on the caller's clock and enforced on the
    // handler's, so compare against a tight window rather than one instant.
    let elapsed = Instant::now().saturating_duration_since(started_at);
    assert!(
        elapsed < Duration::from_secs(60),
        "the poll must finish before the wrapper's hard deadline, took {elapsed:?}"
    );
    assert!(
        nested_budget_window(Duration::from_secs(60)).contains(&elapsed),
        "observation should use the whole budget minus the return margin, took {elapsed:?}"
    );
    assert_eq!(
        output.process_id,
        Some(process_id),
        "a yielded poll returns the live process so the model can poll again"
    );
    assert!(!output.process_exited);

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

/// A short explicit caller timeout outranks the ordinary minimum poll
/// durations, so the call yields cooperatively instead of running past the
/// budget and being cancelled.
#[tokio::test(start_paused = true)]
async fn a_short_nested_budget_outranks_the_minimum_empty_yield() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "short-budget", /*validation*/ false).await?;

    // Well below MIN_EMPTY_YIELD_TIME_MS, which would otherwise floor the wait.
    let nested_deadline = std::time::Instant::now() + Duration::from_secs(3);
    let started_at = Instant::now();
    let output = write_stdin_within(
        &session,
        process_id,
        "",
        /*yield_time_ms*/ 60_000,
        Some(nested_deadline),
    )
    .await?;

    let elapsed = Instant::now().saturating_duration_since(started_at);
    assert!(
        nested_budget_window(Duration::from_secs(3)).contains(&elapsed),
        "the enclosing deadline bounds the wait, not MIN_EMPTY_YIELD_TIME_MS, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(MIN_EMPTY_YIELD_TIME_MS),
        "a {MIN_EMPTY_YIELD_TIME_MS}ms floor would overrun the 3s budget, took {elapsed:?}"
    );
    assert_eq!(output.process_id, Some(process_id));

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

/// An exhausted budget yields immediately with the registered process handle
/// rather than producing a deadline in the past that reads as a failure.
#[tokio::test(start_paused = true)]
async fn an_already_exhausted_nested_budget_yields_immediately() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "spent-budget", /*validation*/ false).await?;

    let started_at = Instant::now();
    let output = write_stdin_within(
        &session,
        process_id,
        "",
        /*yield_time_ms*/ 60_000,
        // Already past by the time the handler reads it.
        Some(std::time::Instant::now()),
    )
    .await?;

    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::ZERO
    );
    assert_eq!(output.process_id, Some(process_id));
    assert!(!output.process_exited);

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

/// Queueing behind another interaction is charged against the same budget. A
/// call that waits out its budget there reports a live process to poll again,
/// never a failure that would strand the process behind a cancelled call.
#[tokio::test(start_paused = true)]
async fn a_poll_queued_past_its_nested_budget_yields_instead_of_failing() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "queued-budget", /*validation*/ false).await?;

    // Hold the interaction lock for longer than the caller's whole budget.
    let held = Arc::clone(&process).interaction_lock().lock_owned().await;

    let nested_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let started_at = Instant::now();
    let output = write_stdin_within(
        &session,
        process_id,
        "",
        /*yield_time_ms*/ 60_000,
        Some(nested_deadline),
    )
    .await?;

    let elapsed = Instant::now().saturating_duration_since(started_at);
    assert!(
        nested_budget_window(Duration::from_secs(10)).contains(&elapsed),
        "the lock wait is bounded by the nested budget, took {elapsed:?}"
    );
    assert_eq!(
        output.process_id,
        Some(process_id),
        "a queued-out poll is resumable, not a failure"
    );
    assert!(!output.process_exited);
    assert!(
        output
            .repair_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("another interaction")),
        "the model needs to know the poll never reached the process: {:?}",
        output.repair_notice
    );

    drop(held);
    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

/// Pushes one burst of output into a live process, then leaves it silent.
fn emit_burst_then_go_silent(process: &Arc<UnifiedExecProcess>, after: Duration) {
    let handles = process.output_handles();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        handles
            .output_buffer
            .lock()
            .await
            .push_chunk(b"   Compiling codex-core\n");
        handles.output_notify.notify_waiters();
    });
}

/// A validation run is silent between bursts of build output. Ending its poll
/// at the first gap is what billed the extra model round trips, so later polls
/// wait for the requested yield instead of the quiet period.
#[tokio::test(start_paused = true)]
async fn an_empty_poll_on_a_validation_launch_waits_the_requested_yield() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "validation-poll", /*validation*/ true).await?;

    emit_burst_then_go_silent(&process, Duration::from_secs(1));
    let started_at = Instant::now();
    let output = write_stdin(&session, process_id, "", /*yield_time_ms*/ 20_000).await?;

    let elapsed = Instant::now().saturating_duration_since(started_at);
    assert_eq!(
        elapsed,
        Duration::from_secs(20),
        "silence after a burst is not a result; the poll keeps the requested yield"
    );
    assert!(
        String::from_utf8_lossy(&output.raw_output).contains("Compiling codex-core"),
        "the burst is still reported: {:?}",
        String::from_utf8_lossy(&output.raw_output)
    );
    assert_eq!(output.process_id, Some(process_id));

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

/// Preservation: an interactive process still ends its poll shortly after
/// output goes quiet, so a prompt returns without waiting out the yield.
#[tokio::test(start_paused = true)]
async fn an_empty_poll_on_an_interactive_process_keeps_the_quiet_period() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "interactive-poll", /*validation*/ false).await?;

    emit_burst_then_go_silent(&process, Duration::from_secs(1));
    let started_at = Instant::now();
    let output = write_stdin(&session, process_id, "", /*yield_time_ms*/ 20_000).await?;

    let elapsed = Instant::now().saturating_duration_since(started_at);
    assert!(
        elapsed < Duration::from_secs(20),
        "an interactive poll must not wait out the full yield, took {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(1),
        "the poll waits for the first output before the quiet period starts, took {elapsed:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.raw_output).contains("Compiling codex-core"),
        "the burst is still reported"
    );

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn noninteractive_poll_keeps_progress_until_completion_or_deadline() -> anyhow::Result<()> {
    for completes in [false, true] {
        let (session, turn) = test_session_and_turn().await;
        let (process_id, process, allow_terminate) =
            register_pollable_process(&session, &turn, "discovery-poll", false).await?;
        session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await
            .processes
            .get_mut(&process_id)
            .expect("registered process")
            .tty = false;
        emit_burst_then_go_silent(&process, Duration::from_secs(1));
        if completes {
            let process = Arc::clone(&process);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let handles = process.output_handles();
                handles.output_buffer.lock().await.push_chunk(b"complete\n");
                process.signal_exit_for_test(Some(0));
                handles
                    .output_closed
                    .store(true, std::sync::atomic::Ordering::Release);
                handles.output_closed_notify.notify_waiters();
            });
        }
        let started_at = Instant::now();
        let output = write_stdin(&session, process_id, "", 20_000).await?;
        assert_eq!(
            Instant::now() - started_at,
            Duration::from_secs(if completes { 3 } else { 20 }),
            "progress must not cause an extra model poll"
        );
        assert!(String::from_utf8_lossy(&output.raw_output).contains("Compiling codex-core"));
        assert_eq!(
            output.process_id,
            if completes { None } else { Some(process_id) }
        );
        assert_eq!(output.exit_code, if completes { Some(0) } else { None });
        if completes {
            assert!(String::from_utf8_lossy(&output.raw_output).contains("complete"));
        }
        session
            .services
            .unified_exec_manager
            .release_process_id(process_id)
            .await;
        allow_terminate.notify_one();
        process.terminate();
    }
    Ok(())
}

/// Preservation: a direct model call has no wrapper bounding it, so the
/// configured background maximum still governs the wait.
#[tokio::test(start_paused = true)]
async fn a_direct_call_without_a_nested_budget_keeps_the_background_maximum() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let (process_id, process, allow_terminate) =
        register_pollable_process(&session, &turn, "direct-poll", /*validation*/ false).await?;

    let started_at = Instant::now();
    let output = write_stdin_within(
        &session, process_id, "", /*yield_time_ms*/ 600_000, /*nested_deadline*/ None,
    )
    .await?;

    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::from_secs(300)
    );
    assert_eq!(output.process_id, Some(process_id));

    session
        .services
        .unified_exec_manager
        .release_process_id(process_id)
        .await;
    allow_terminate.notify_one();
    process.terminate();
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn write_stdin_yield_deadlines_include_reaction_and_cap_background_wait() -> anyhow::Result<()>
{
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, _terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id: Default::default(),
            search_exit_one_is_no_match: false,
            parent_tool_execution_id: Default::default(),
            call_id: "write-stdin-deadline".to_string(),
            process_id,
            cwd: turn.cwd().clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "interactive".to_string(),
            tty: true,
            validation_launch: false,
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
        },
    );

    let started_at = Instant::now();
    let output = write_stdin(
        &session,
        process_id,
        "input",
        /*yield_time_ms*/ MIN_YIELD_TIME_MS,
    )
    .await?;

    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::from_millis(MIN_YIELD_TIME_MS)
    );
    assert_eq!(output.wall_time, Duration::from_millis(MIN_YIELD_TIME_MS));
    assert_eq!(output.process_id, Some(process_id));

    let started_at = Instant::now();
    let output = write_stdin(&session, process_id, "", /*yield_time_ms*/ 0).await?;
    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::from_millis(MIN_YIELD_TIME_MS)
    );
    assert_eq!(output.wall_time, Duration::from_millis(MIN_YIELD_TIME_MS));
    assert!(output.raw_output.is_empty());
    assert_eq!(output.process_id, Some(process_id));
    assert_eq!(output.exit_code, None);
    assert!(!output.process_exited);

    let started_at = Instant::now();
    let output = write_stdin(&session, process_id, "", /*yield_time_ms*/ 600_000).await?;
    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::from_secs(300)
    );
    assert_eq!(output.wall_time, Duration::from_secs(300));
    assert!(output.raw_output.is_empty());
    assert_eq!(output.process_id, Some(process_id));
    assert_eq!(output.exit_code, None);
    assert!(!output.process_exited);

    let output = write_stdin(
        &session, process_id, "input", /*yield_time_ms*/ 300_000,
    )
    .await?;
    assert_eq!(output.wall_time, Duration::from_secs(30));
    assert_eq!(output.process_id, Some(process_id));

    manager.release_process_id(process_id).await;
    allow_terminate.notify_one();
    process.terminate();

    Ok(())
}

#[test]
fn push_chunk_preserves_prefix_and_suffix() {
    let mut buffer = HeadTailBuffer::default();
    buffer.push_chunk(&vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(b"b");
    buffer.push_chunk(b"c");

    assert_eq!(buffer.retained_bytes(), UNIFIED_EXEC_OUTPUT_MAX_BYTES);
    let snapshot = buffer.snapshot_chunks();

    let mut expected = vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES - 2];
    expected.extend_from_slice(b"bc");
    assert_eq!(snapshot.concat(), expected);
    assert_eq!(buffer.omitted_bytes(), 2);
}

#[test]
fn head_tail_buffer_default_preserves_prefix_and_suffix() {
    let mut buffer = HeadTailBuffer::default();
    buffer.push_chunk(&vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(b"bc");

    let rendered = buffer.to_bytes();
    let mut expected = vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES - 2];
    expected.extend_from_slice(b"bc");
    assert_eq!(rendered, expected);
    assert_eq!(buffer.omitted_bytes(), 2);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_persists_across_requests() -> anyhow::Result<()> {
    let (session, mut turn) = test_session_and_turn().await;
    // This fixture has no approval responder; explicitly authorize its interactive shell.
    Arc::get_mut(&mut turn)
        .expect("turn is uniquely owned")
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)?;
    let cwd = turn.cwd().clone();

    let open_shell = exec_command(
        &session,
        &turn,
        "powershell.exe -NoExit",
        /*yield_time_ms*/ 2_500,
        /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell
        .process_id
        .unwrap_or_else(|| panic!("interactive shell exited before input: {open_shell:?}"));
    assert_eq!(
        session.list_background_terminals().await,
        vec![BackgroundTerminalInfo {
            item_id: "call".to_string(),
            process_id: process_id.to_string(),
            command: "powershell.exe -NoExit".to_string(),
            cwd: cwd.into(),
        }]
    );

    write_stdin(
        &session,
        process_id,
        "$env:CODEX_INTERACTIVE_SHELL_VAR = 'codex'\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        process_id,
        "Write-Output $env:CODEX_INTERACTIVE_SHELL_VAR\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    assert!(
        out_2
            .truncated_output(TEST_MAX_OUTPUT_TOKENS)
            .contains("codex"),
        "expected environment variable output"
    );

    assert!(session.terminate_background_terminal(process_id).await);
    assert!(!session.terminate_background_terminal(process_id).await);
    assert!(session.list_background_terminals().await.is_empty());

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_dispatch_reuses_workspace_baseline_without_ledger_recapture() -> anyhow::Result<()>
{
    for nested in [false, true] {
        let fixture = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(fixture.path())
                .status()?
                .success()
        );
        std::fs::write(
            fixture.path().join("evidence.txt"),
            "workspace-baseline-output",
        )?;
        let (mut session, mut turn) = make_session_and_context().await;
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        let mut config = (*turn.config).clone();
        config.cwd = AbsolutePathBuf::from_absolute_path(fixture.path())?;
        config
            .features
            .enable(codex_features::Feature::UnifiedExec)?;
        config
            .features
            .enable(codex_features::Feature::Kd4Runtime)?;
        config.permissions.approval_policy =
            crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
        session.services.command_execution =
            crate::tools::command_execution::CommandExecutionLedger::load_or_new(
                config.codex_home.to_path_buf(),
                "baseline-test".to_string(),
                fixture.path(),
            )
            .await;
        turn.config = Arc::new(config);
        let session = Arc::new(session);
        let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
        let router = Arc::new(crate::tools::router::ToolRouter::from_context(
            step.as_ref(),
            crate::tools::router::ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        assert!(step.set_tool_router(router).is_ok());
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            Arc::clone(&session),
            step,
            Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
        );
        let call = crate::tools::router::ToolCall {
            tool_name: codex_tools::ToolName::plain("exec_command"),
            call_id: "baseline-read".to_string(),
            payload: crate::tools::context::ToolPayload::Function {
                arguments: serde_json::json!({
                    "cmd": "Get-Content 'evidence.txt'", "workdir": fixture.path(),
                    "yield_time_ms": 1000, "tty": false,
                })
                .to_string(),
            },
        };
        let response = if nested {
            runtime
                .handle_tool_call_with_source(
                    call,
                    crate::tools::context::ToolCallSource::CodeMode {
                        cell_id: "cell-1".to_string(),
                        parent_call_id: Some("outer".to_string()),
                        runtime_tool_call_id: "nested-read".to_string(),
                        nested_deadline: None,
                        cancellation_cause: None,
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await?
                .response()
        } else {
            runtime
                .handle_tool_call(call, tokio_util::sync::CancellationToken::new())
                .await?
        };
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response
        else {
            panic!("expected command output: {response:?}");
        };
        let text = output.body.to_text().expect("model-visible command output");
        assert!(text.starts_with("Process exited with code 0;"), "{text}");
        assert!(text.contains("workspace-baseline-output"), "{text}");
        assert!(
            session
                .services
                .command_execution
                .current_workspace_identity_hash(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    fixture.path(),
                )
                .await
                .is_some(),
            "dispatch must initialize the ledger identity"
        );
        assert_eq!(
            session
                .services
                .command_execution
                .workspace_identity_capture_count(),
            0,
            "direct and nested execution must use dispatch's snapshot"
        );
        assert!(session.list_background_terminals().await.is_empty());
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_wait_delivers_exit_after_progress_and_preserves_deadline() -> anyhow::Result<()>
{
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(codex_features::Feature::UnifiedExec)?;
    config.permissions.approval_policy =
        crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
    let router = Arc::new(crate::tools::router::ToolRouter::from_context(
        step.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step.set_tool_router(router).is_ok());
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        Arc::clone(&session),
        step,
        Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
    );
    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("test_wait.py");
    for (case, delay, passes) in [
        ("argv", 0.7, true),
        ("compound", 0.7, true),
        ("failure", 0.7, false),
        ("deadline", 30.0, true),
    ] {
        std::fs::write(
            &script,
            format!(
                "import time, unittest\nclass WaitTest(unittest.TestCase):\n    def test_result(self):\n        print('VALIDATION_STARTED', flush=True)\n        time.sleep({delay})\n        self.assertEqual({}, True)\n        print('VALIDATION_FINISHED', flush=True)\n",
                if passes { "True" } else { "False" },
            ),
        )?;
        let mut arguments = if case == "compound" {
            serde_json::json!({"cmd": "Get-Content test_wait.py; python -u -m unittest -q", "shell": "powershell.exe"})
        } else {
            serde_json::json!({"kind": "argv", "program": "python", "args": ["-u", "-m", "unittest", "-q"]})
        };
        arguments["workdir"] = serde_json::json!(fixture.path());
        arguments["yield_time_ms"] = serde_json::json!(3000);
        arguments["tty"] = serde_json::json!(false);
        arguments["force_fresh"] = serde_json::json!(true);
        let response = runtime
            .clone()
            .handle_tool_call(
                crate::tools::router::ToolCall {
                    tool_name: codex_tools::ToolName::plain("exec_command"),
                    call_id: format!("validation-wait-{case}"),
                    payload: crate::tools::context::ToolPayload::Function {
                        arguments: arguments.to_string(),
                    },
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput { output, .. } = response
        else {
            panic!("expected command output: {response:?}");
        };
        let text = output.body.to_text().expect("model-visible output");
        assert!(text.contains("VALIDATION_STARTED"), "{case}: {text}");
        if case == "deadline" {
            let terminals = session.list_background_terminals().await;
            assert_eq!(terminals.len(), 1, "{text}");
            let process_id = terminals[0].process_id.parse::<u32>()?;
            assert!(session.terminate_background_terminal(process_id).await);
            assert!(
                text.starts_with("Process running with session ID"),
                "{text}"
            );
            assert!(!text.contains("VALIDATION_FINISHED"), "{text}");
        } else {
            let exit_code = if passes { 0 } else { 1 };
            assert!(
                text.starts_with(&format!("Process exited with code {exit_code};")),
                "{case}: {text}"
            );
            assert!(
                !text.contains("Process running with session ID"),
                "{case}: {text}"
            );
            assert!(
                text.contains(if passes { "OK" } else { "FAILED (failures=1)" }),
                "{case}: {text}"
            );
            assert!(session.list_background_terminals().await.is_empty());
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_unified_exec_sessions() -> anyhow::Result<()> {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(codex_features::Feature::UnifiedExec)?;
    config.permissions.approval_policy =
        crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
    let router = Arc::new(crate::tools::router::ToolRouter::from_context(
        step.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step.set_tool_router(router).is_ok());
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        Arc::clone(&session),
        step,
        Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
    );
    assert!(runtime.has_registered_tool(&codex_tools::ToolName::plain("exec_command")));
    assert!(runtime.has_registered_tool(&codex_tools::ToolName::plain("write_stdin")));

    // This helper only dispatches registered calls and extracts their real
    // model-visible payload; it does not create processes or synthesize status.
    async fn dispatch(
        runtime: &crate::tools::parallel::ToolCallRuntime,
        tool_name: &str,
        call_id: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<(Option<bool>, String)> {
        let response = runtime
            .clone()
            .handle_tool_call(
                crate::tools::router::ToolCall {
                    tool_name: codex_tools::ToolName::plain(tool_name),
                    call_id: call_id.to_string(),
                    payload: crate::tools::context::ToolPayload::Function {
                        arguments: arguments.to_string(),
                    },
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        let codex_protocol::models::ResponseInputItem::FunctionCallOutput {
            call_id: returned_call_id,
            output,
        } = response
        else {
            panic!("expected registered function output: {response:?}")
        };
        assert_eq!(returned_call_id, call_id);
        Ok((
            output.success,
            output.body.to_text().expect("textual command output"),
        ))
    }

    let (_, opened) = dispatch(
        &runtime,
        "exec_command",
        "multi-shell-a",
        serde_json::json!({
            "kind": "argv", "program": "powershell.exe",
            "args": ["-NoLogo", "-NoProfile", "-NoExit"],
            "tty": true, "yield_time_ms": 2500,
        }),
    )
    .await?;
    let terminals = session.list_background_terminals().await;
    assert_eq!(
        terminals.len(),
        1,
        "registered shell must remain alive: {opened}"
    );
    let process_id = terminals[0].process_id.parse::<u32>()?;
    assert!(
        opened.contains(&format!("Process running with session ID {process_id};")),
        "{opened}"
    );

    // A unique name makes isolation independent of the host environment.
    let variable = format!("CODEX_MULTI_SESSION_{}", uuid::Uuid::new_v4().simple());
    let value = "codex-session-state";
    dispatch(
        &runtime,
        "write_stdin",
        "multi-set-a",
        serde_json::json!({
            "session_id": process_id,
            "chars": format!("$env:{variable} = '{value}'\n"),
            "yield_time_ms": 2500,
        }),
    )
    .await?;

    let (short_success, short_output) = dispatch(
        &runtime,
        "exec_command",
        "multi-fresh-shell",
        serde_json::json!({
            "kind": "argv", "program": "powershell.exe",
            "args": ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                format!("Write-Output ('FRESH:' + $env:{variable} + ':DONE')")],
            "tty": false, "yield_time_ms": 30_000,
        }),
    )
    .await?;
    assert_eq!(
        short_success,
        Some(true),
        "fresh shell failed: {short_output}"
    );
    assert!(
        short_output.starts_with("Process exited with code 0;"),
        "short command must complete inline: {short_output}"
    );
    assert!(
        !short_output.contains("Process running with session ID"),
        "completed command must not return a live session: {short_output}"
    );
    let normalized = short_output.replace("\r\n", "\n");
    let (_, fresh_output) = normalized
        .split_once("\nOutput:\n")
        .expect("real output section");
    assert_eq!(
        fresh_output.trim(),
        "FRESH::DONE",
        "fresh shell must not inherit A's state"
    );
    assert!(!fresh_output.contains(value));
    let remaining = session.list_background_terminals().await;
    assert_eq!(
        remaining.len(),
        1,
        "inline command must not leave a second process"
    );
    assert_eq!(remaining[0].process_id, process_id.to_string());
    {
        let store = session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await;
        assert_eq!(store.processes.len(), 1);
        assert!(store.processes.contains_key(&process_id));
        assert_eq!(store.reserved_process_ids.len(), 1);
        assert!(store.reserved_process_ids.contains(&process_id));
    }

    let (_, preserved) = dispatch(
        &runtime,
        "write_stdin",
        "multi-read-a",
        serde_json::json!({
            "session_id": process_id,
            // The expected contiguous marker never occurs in the echoed input.
            "chars": format!("Write-Output ('PERSISTED:' + $env:{variable})\n"),
            "yield_time_ms": 2500,
        }),
    )
    .await?;
    assert!(
        preserved.contains(&format!("Process running with session ID {process_id};")),
        "{preserved}"
    );
    assert!(
        preserved.contains("PERSISTED:codex-session-state"),
        "A must preserve its own state: {preserved}"
    );

    assert!(session.terminate_background_terminal(process_id).await);
    assert!(session.list_background_terminals().await.is_empty());
    assert!(
        session
            .services
            .command_execution
            .running_process(process_id)
            .await
            .is_none()
    );
    let store = session
        .services
        .unified_exec_manager
        .process_store
        .lock()
        .await;
    assert!(store.processes.is_empty());
    assert!(store.reserved_process_ids.is_empty());
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn unified_exec_silent_command_finishes_within_requested_initial_yield() -> anyhow::Result<()>
{
    let (session, mut turn) = test_session_and_turn().await;
    Arc::get_mut(&mut turn)
        .expect("turn is uniquely owned")
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)?;
    let result = exec_command_with_tty(
        &session,
        &turn,
        "Start-Sleep -Milliseconds 750; Write-Output 'silent-command-complete'",
        10_000,
        None,
        false,
    )
    .await?;
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result.process_id.is_none(),
        "finished command must not require polling"
    );
    assert_eq!(
        result.truncated_output(TEST_MAX_OUTPUT_TOKENS).trim(),
        "silent-command-complete"
    );
    assert!(session.list_background_terminals().await.is_empty());
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn unified_exec_noninteractive_bursts_finish_in_one_initial_call() -> anyhow::Result<()> {
    let (session, mut turn) = test_session_and_turn().await;
    Arc::get_mut(&mut turn)
        .expect("turn is uniquely owned")
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::Never)?;
    let result = exec_command_with_tty(
        &session,
        &turn,
        "Write-Output 'first'; Start-Sleep -Milliseconds 1000; Write-Output 'last'",
        10_000,
        None,
        false,
    )
    .await?;
    assert_eq!(result.exit_code, Some(0));
    assert!(
        result.process_id.is_none(),
        "progress must not require a model poll"
    );
    let output = result.truncated_output(TEST_MAX_OUTPUT_TOKENS);
    assert_eq!(output.lines().collect::<Vec<_>>(), vec!["first", "last"]);
    assert!(session.list_background_terminals().await.is_empty());
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_timeouts() -> anyhow::Result<()> {
    const TEST_VAR_VALUE: &str = "unified_exec_var_123";

    let (session, turn) = test_session_and_turn().await;

    let open_shell = exec_command(
        &session,
        &turn,
        r#"powershell.exe -NoLogo -NoProfile -NoExit -Command "Remove-Module PSReadLine -ErrorAction SilentlyContinue""#,
        /*yield_time_ms*/ 2_500,
        /*workdir*/ None,
    )
    .await?;
    let process_id = open_shell
        .process_id
        .unwrap_or_else(|| panic!("interactive shell exited before input: {open_shell:?}"));

    write_stdin(
        &session,
        process_id,
        format!("$env:CODEX_INTERACTIVE_SHELL_VAR = '{TEST_VAR_VALUE}'\r\n").as_str(),
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        process_id,
        "Start-Sleep -Seconds 5; Write-Output $env:CODEX_INTERACTIVE_SHELL_VAR\r\n",
        /*yield_time_ms*/ 10,
    )
    .await?;
    assert!(
        !out_2
            .truncated_output(TEST_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "timeout too short should yield incomplete output"
    );
    assert_eq!(out_2.process_id, Some(process_id));
    assert_eq!(out_2.exit_code, None);

    let out_3 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = write_stdin(&session, process_id, "", 1_000).await?;
            if output
                .truncated_output(TEST_MAX_OUTPUT_TOKENS)
                .contains(TEST_VAR_VALUE)
            {
                return Ok::<_, UnifiedExecError>(output);
            }
        }
    })
    .await??;

    assert!(
        out_3
            .truncated_output(TEST_MAX_OUTPUT_TOKENS)
            .contains(TEST_VAR_VALUE),
        "subsequent poll should retrieve output"
    );
    assert!(session.terminate_background_terminal(process_id).await);
    assert!(session.list_background_terminals().await.is_empty());

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_pause_blocks_yield_timeout() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let elicitation = session.services.elicitations.register();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(elicitation);
    });

    let started = tokio::time::Instant::now();
    let response = exec_command(
        &session,
        &turn,
        "Start-Sleep -Seconds 1; Write-Output unified-exec-done",
        /*yield_time_ms*/ 250,
        /*workdir*/ None,
    )
    .await?;

    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "pause should block the unified exec yield timeout"
    );
    assert!(
        response
            .truncated_output(TEST_MAX_OUTPUT_TOKENS)
            .contains("unified-exec-done"),
        "exec_command should wait for output after the pause lifts"
    );
    assert!(
        response.process_id.is_none(),
        "completed command should not leave a background process"
    );

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_completed_process_returns_unknown_process() -> anyhow::Result<()> {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(codex_features::Feature::UnifiedExec)?;
    config.permissions.approval_policy =
        crate::config::Constrained::allow_any(codex_protocol::protocol::AskForApproval::Never);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
    let router = Arc::new(crate::tools::router::ToolRouter::from_context(
        step.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    assert!(step.set_tool_router(router).is_ok());
    let runtime = crate::tools::parallel::ToolCallRuntime::new(
        session.clone(),
        step,
        Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
    );
    let opened = runtime
        .handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: "interactive-cleanup".to_string(),
                payload: crate::tools::context::ToolPayload::Function {
                    arguments: serde_json::json!({
                        "kind": "argv",
                        "program": "powershell.exe",
                        "args": ["-NoLogo", "-NoProfile", "-NoExit"],
                        "tty": true,
                        "yield_time_ms": 2500
                    })
                    .to_string(),
                },
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await?;
    let terminals = session.list_background_terminals().await;
    assert_eq!(
        terminals.len(),
        1,
        "registered shell must stay alive: {opened:?}"
    );
    let process_id = terminals[0].process_id.parse::<u32>()?;

    let mut closed = write_stdin(
        &session,
        process_id,
        "Write-Output ('final-' + 'output-preserved'); exit\n",
        /*yield_time_ms*/ 2_500,
    )
    .await?;
    let mut final_output = closed.raw_output.clone();
    assert_eq!(closed.exit_code, Some(0), "shell did not exit: {closed:?}");
    assert!(closed.process_exited);
    // Exit status may arrive before the PTY output closes. Consume the retained
    // session until the public response confirms that its output has drained.
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(retained_id) = closed.process_id {
            assert_eq!(retained_id, process_id);
            closed = write_stdin(&session, process_id, "", /*yield_time_ms*/ 100).await?;
            final_output.extend_from_slice(&closed.raw_output);
            assert_eq!(closed.exit_code, Some(0));
            assert!(closed.process_exited);
            tokio::task::yield_now().await;
        }
        Ok::<(), UnifiedExecError>(())
    })
    .await
    .expect("exited shell output must close and release its process id")?;
    assert_eq!(closed.process_id, None);
    assert!(
        String::from_utf8_lossy(&final_output).contains("final-output-preserved"),
        "final command output must survive PTY cleanup: {:?}",
        String::from_utf8_lossy(&final_output)
    );

    let err = write_stdin(&session, process_id, "", /*yield_time_ms*/ 100)
        .await
        .expect_err("expected unknown process error");

    match err {
        UnifiedExecError::UnknownProcessId { process_id: err_id } => {
            assert_eq!(err_id, process_id, "process id should match request");
        }
        other => panic!("expected UnknownProcessId, got {other:?}"),
    }

    assert!(
        session
            .services
            .unified_exec_manager
            .process_store
            .lock()
            .await
            .processes
            .is_empty()
    );
    assert!(session.list_background_terminals().await.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminating_initial_exec_command_rechecks_initial_response_state() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, mut terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    let cwd = turn.cwd().clone();
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process,
            command_execution_id: Default::default(),
            search_exit_one_is_no_match: false,
            parent_tool_execution_id: Default::default(),
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: "Start-Sleep -Seconds 60".to_string(),
            tty: true,
            validation_launch: false,
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used: Instant::now(),
        },
    );

    let terminate_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.terminate_background_terminal(process_id).await }
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        terminate_started_rx.wait_for(|started| *started),
    )
    .await
    .expect("terminate should start")
    .expect("terminate signal sender should stay open");

    {
        let mut store = manager.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .expect("process should remain stored until initial response returns");
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    allow_terminate.notify_waiters();
    let terminated = tokio::time::timeout(Duration::from_secs(2), terminate_task)
        .await
        .expect("terminate should finish")
        .expect("terminate task should not panic");
    assert!(terminated);
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&process_id)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminating_during_stdin_poll_returns_exited_response() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let (terminate_started_tx, _terminate_started_rx) = watch::channel(false);
    let allow_terminate = Arc::new(Notify::new());
    let process = blocking_terminate_unified_process(
        process_id,
        terminate_started_tx,
        Arc::clone(&allow_terminate),
    )
    .await?;
    let cwd = turn.cwd().clone();
    let last_used = Instant::now() - Duration::from_secs(1);
    manager.process_store.lock().await.processes.insert(
        process_id,
        ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id: Default::default(),
            search_exit_one_is_no_match: false,
            parent_tool_execution_id: Default::default(),
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "Start-Sleep -Seconds 60".to_string(),
            tty: true,
            validation_launch: false,
            network_approval: None,
            session: Arc::downgrade(&session),
            last_used,
        },
    );

    let poll_task = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            write_stdin(&session, process_id, "", /*yield_time_ms*/ 60_000).await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let poll_started = manager
                .process_store
                .lock()
                .await
                .processes
                .get(&process_id)
                .is_some_and(|entry| entry.last_used != last_used);
            if poll_started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("poll should clone process handles");

    manager.release_process_id(process_id).await;
    allow_terminate.notify_one();
    process.terminate_confirmed().await?;

    let output = tokio::time::timeout(Duration::from_secs(2), poll_task)
        .await
        .expect("poll should finish")
        .expect("poll task should not panic")?;
    assert_eq!(output.process_id, None);
    assert!(manager.process_store.lock().await.processes.is_empty());

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_pipe_commands_preserve_exit_code() -> anyhow::Result<()> {
    let (_, turn) = make_session_and_context().await;
    let cwd = turn.cwd().clone();
    let request = test_exec_request(
        &turn,
        vec![
            "cmd.exe".to_string(),
            "/D".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            "exit /b 17".to_string(),
        ],
        cwd,
        shell_env(),
    );

    let environment = codex_exec_server::Environment::default_for_tests();
    let process = UnifiedExecProcessManager::default()
        .open_session_with_prepared_exec_env(
            /*process_id*/ 1234,
            &request,
            /*tty*/ false,
            Box::new(NoopSpawnLifecycle),
            None,
            &environment,
            &PendingSpawnRegistration::default(),
        )
        .await?;

    if !process.has_exited() {
        let exit_signal = process.cancellation_token();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), exit_signal.cancelled())
                .await
                .is_ok(),
            "process did not report exit within timeout"
        );
    }

    assert!(process.has_exited());
    assert_eq!(process.exit_code(), Some(17));
    Ok(())
}
