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
use crate::unified_exec::process::OutputHandles;
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
use codex_utils_output_truncation::approx_token_count;
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
    let (session, turn) = make_session_and_context().await;
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
    let process_id = manager.allocate_process_id().await;
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
    let request = test_exec_request(turn, command.clone(), cwd.clone(), shell_env());

    let process = manager
        .open_session_with_prepared_exec_env(
            process_id,
            &request,
            tty,
            Box::new(NoopSpawnLifecycle),
            None,
            turn.environments
                .primary()
                .expect("turn environment")
                .environment
                .as_ref(),
            &PendingSpawnRegistration::default(),
        )
        .await?;
    let context =
        UnifiedExecContext::new(Arc::clone(session), Arc::clone(turn), "call".to_string());
    let started_at = Instant::now();
    let process_started_alive = !process.has_exited() && process.exit_code().is_none();
    if process_started_alive {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            command_execution_id: Default::default(),
            parent_tool_execution_id: Default::default(),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: cmd.to_string(),
            tty,
            network_approval: None,
            session: Arc::downgrade(session),
            last_used: started_at,
        };
        manager
            .process_store
            .lock()
            .await
            .processes
            .insert(process_id, entry);
    }

    let OutputHandles {
        output_buffer,
        output_notify,
        output_closed,
        output_closed_notify,
        cancellation_token,
        ..
    } = process.output_handles();
    let deadline = started_at + Duration::from_millis(yield_time_ms);
    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output_buffer,
        &output_notify,
        &output_closed,
        &output_closed_notify,
        &cancellation_token,
        Some(session.subscribe_elicitation_pause_state()),
        deadline,
    )
    .await;
    let wall_time = Instant::now().saturating_duration_since(started_at);
    let text = String::from_utf8_lossy(&collected).to_string();
    let has_exited = process.has_exited();
    let exit_code = process.exit_code();
    let response_process_id = if process_started_alive && !has_exited {
        Some(process_id)
    } else {
        manager.release_process_id(process_id).await;
        None
    };
    if response_process_id.is_some()
        && let Some(entry) = manager
            .process_store
            .lock()
            .await
            .processes
            .get_mut(&process_id)
    {
        entry
            .initial_exec_command_active
            .store(false, std::sync::atomic::Ordering::Release);
    }

    Ok(ExecCommandToolOutput {
        validation: None,
        event_call_id: context.call_id,
        chunk_id: generate_chunk_id(),
        wall_time,
        raw_output: collected,
        truncation_policy: turn.model_info.truncation_policy.into(),
        max_output_tokens: None,
        process_id: response_process_id,
        exit_code,
        process_exited: exit_code.is_some(),
        original_token_count: Some(approx_token_count(&text)),
        hook_command: Some(cmd.to_string()),
        raw_output_artifact: None,
        raw_output_reduction_notice: None,
        repair_notice: None,
    })
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
    session
        .services
        .unified_exec_manager
        .write_stdin(WriteStdinRequest {
            process_id,
            input,
            yield_time_ms,
            max_output_tokens: None,
            truncation_policy: TruncationPolicy::Tokens(10_000),
        })
        .await
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
            parent_tool_execution_id: Default::default(),
            call_id: "write-stdin-deadline".to_string(),
            process_id,
            cwd: turn.cwd().clone().into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "interactive".to_string(),
            tty: true,
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
    let output = write_stdin(&session, process_id, "", /*yield_time_ms*/ 120_000).await?;
    assert_eq!(
        Instant::now().saturating_duration_since(started_at),
        Duration::from_secs(60)
    );
    assert_eq!(output.wall_time, Duration::from_secs(60));
    assert!(output.raw_output.is_empty());
    assert_eq!(output.process_id, Some(process_id));
    assert_eq!(output.exit_code, None);
    assert!(!output.process_exited);

    manager.release_process_id(process_id).await;
    allow_terminate.notify_one();
    process.terminate();

    Ok(())
}

#[test]
fn push_chunk_preserves_prefix_and_suffix() {
    let mut buffer = HeadTailBuffer::default();
    buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(vec![b'b']);
    buffer.push_chunk(vec![b'c']);

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
    buffer.push_chunk(vec![b'a'; UNIFIED_EXEC_OUTPUT_MAX_BYTES]);
    buffer.push_chunk(b"bc".to_vec());

    let rendered = buffer.to_bytes();
    assert_eq!(rendered.first(), Some(&b'a'));
    assert!(rendered.ends_with(b"bc"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_persists_across_requests() -> anyhow::Result<()> {
    let (session, turn) = test_session_and_turn().await;
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

#[tokio::test]
async fn unified_exec_timeouts() -> anyhow::Result<()> {
    const TEST_VAR_VALUE: &str = "unified_exec_var_123";

    let (session, turn) = test_session_and_turn().await;

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

    write_stdin(
        &session,
        process_id,
        format!("$env:CODEX_INTERACTIVE_SHELL_VAR = '{TEST_VAR_VALUE}'\n").as_str(),
        /*yield_time_ms*/ 2_500,
    )
    .await?;

    let out_2 = write_stdin(
        &session,
        process_id,
        "Start-Sleep -Seconds 5; Write-Output $env:CODEX_INTERACTIVE_SHELL_VAR\n",
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

    tokio::time::sleep(Duration::from_secs(7)).await;

    let out_3 = write_stdin(&session, process_id, "", /*yield_time_ms*/ 100).await?;

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
            parent_tool_execution_id: Default::default(),
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            hook_command: "Start-Sleep -Seconds 60".to_string(),
            tty: true,
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
            parent_tool_execution_id: Default::default(),
            call_id: "call".to_string(),
            process_id,
            cwd: cwd.into(),
            initial_exec_command_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            hook_command: "Start-Sleep -Seconds 60".to_string(),
            tty: true,
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
