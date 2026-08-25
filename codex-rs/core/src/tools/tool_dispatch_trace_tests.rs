use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::ThreadStartedTraceMetadata;
use codex_rollout_trace::ToolCallRequester;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::code_mode::CodeModeWaitHandler;
use crate::tools::code_mode::WAIT_TOOL_NAME;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::TurnTimingState;

#[test]
fn tool_lifecycle_uses_one_clock_and_records_all_boundaries() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing = ToolDispatchTiming::new_with_turn_clock(
        Arc::clone(&turn_timing),
        tokio::time::Instant::now(),
        false,
    );
    turn_timing.adjust_parallel_gate_waiters(1);
    timing.mark_parallel_gate_admitted();
    turn_timing.adjust_parallel_gate_waiters(-1);
    turn_timing.adjust_active_tools(1);
    timing.mark_handler_entry();
    timing.mark_exec_process_spawned();
    timing.mark_exec_process_exited();
    timing.increment_retry_count();
    timing.mark_handler_entry();
    timing.mark_handler_exit();
    turn_timing.adjust_active_tools(-1);
    timing.mark_output_collected();
    assert!(timing.mark_relay_enqueue());
    let execution_id = timing.execution_id().clone();
    assert!(timing.mark_relay_delivery(&execution_id));
    timing.mark_next_model_sample_start();

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    let boundaries = snapshot
        .lifecycle_events
        .iter()
        .map(|event| event.boundary)
        .collect::<Vec<_>>();
    assert_eq!(
        boundaries,
        vec![
            ToolLifecycleBoundary::RequestCreated,
            ToolLifecycleBoundary::Admitted,
            ToolLifecycleBoundary::HandlerStart,
            ToolLifecycleBoundary::ProcessSpawn,
            ToolLifecycleBoundary::ProcessExit,
            ToolLifecycleBoundary::HandlerReturn,
            ToolLifecycleBoundary::RelayEnqueue,
            ToolLifecycleBoundary::RelayDelivery,
            ToolLifecycleBoundary::NextModelSampleStart,
        ]
    );
    assert_eq!(snapshot.retry_count, 1);
    assert_eq!(snapshot.reentry_count, 1);
    assert_eq!(
        snapshot
            .lifecycle_events
            .iter()
            .find(|event| event.boundary == ToolLifecycleBoundary::Admitted)
            .expect("admission event")
            .context
            .parallel_gate_waiter_count,
        1
    );
    assert_eq!(turn_timing.lifecycle_context().relay_queue_depth, 0);
}

#[test]
fn process_wait_records_timeout_wake_and_reentry() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing =
        ToolDispatchTiming::new_with_turn_clock(turn_timing, tokio::time::Instant::now(), false);
    timing.mark_handler_entry();
    timing.mark_handler_entry();
    timing.increment_retry_count();
    timing.record_timer_wait(ToolLifecycleTimerWait {
        wait_kind: "owner_wait".to_string(),
        requested_timeout_ms: Some(60_000),
        effective_timeout_ms: Some(60_000),
        deadline_at_ms: timing.deadline_after_ms(60_000),
        wake_reason: ToolLifecycleWakeReason::Timeout,
        sequence: 0,
    });

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(snapshot.retry_count, 1);
    assert_eq!(snapshot.reentry_count, 1);
    assert_eq!(snapshot.timer_waits.len(), 1);
    assert_eq!(snapshot.timer_waits[0].sequence, 1);
    assert_eq!(snapshot.timer_waits[0].deadline_at_ms, Some(60_000));
    assert_eq!(
        snapshot.timer_waits[0].wake_reason,
        ToolLifecycleWakeReason::Timeout
    );
}

#[tokio::test(start_paused = true)]
async fn dispatch_timing_separates_item_poll_gate_authorization_and_handler_boundaries() {
    let accepted_at = tokio::time::Instant::now();
    let timing = ToolDispatchTiming::new(accepted_at, /*eager*/ true);

    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    timing.mark_first_poll();
    tokio::time::advance(std::time::Duration::from_millis(30)).await;
    timing.mark_parallel_gate_admitted();
    timing.record_authorization_state_coordination(std::time::Duration::from_millis(17));
    tokio::time::advance(std::time::Duration::from_millis(40)).await;
    timing.mark_handler_entry();
    tokio::time::advance(std::time::Duration::from_millis(10)).await;
    timing.mark_handler_exit();
    tokio::time::advance(std::time::Duration::from_millis(5)).await;
    timing.mark_output_collected();
    timing.record_workspace_evidence_before(std::time::Duration::from_millis(3));
    timing.record_workspace_evidence_before_attribution(
        false,
        vec!["worktree".to_string(), "untracked".to_string()],
    );
    timing.record_workspace_evidence_after(std::time::Duration::from_millis(4));

    let snapshot = timing.snapshot(tokio::time::Instant::now());
    assert!(snapshot.eager);
    assert_eq!(snapshot.item_to_first_poll_ms, Some(20));
    assert_eq!(snapshot.parallel_gate_wait_ms, Some(30));
    assert_eq!(snapshot.authorization_state_coordination_ms, Some(17));
    assert_eq!(snapshot.first_poll_to_handler_entry_ms, Some(70));
    assert_eq!(snapshot.handler_duration_ms, Some(10));
    assert_eq!(snapshot.first_poll_to_output_collected_ms, Some(85));
    assert_eq!(snapshot.workspace_evidence_before_ms, Some(3));
    assert_eq!(snapshot.workspace_evidence_before_cache_hit, Some(false));
    assert_eq!(
        snapshot.workspace_evidence_before_timed_out_git_dependencies,
        vec!["worktree", "untracked"]
    );
    assert_eq!(snapshot.workspace_evidence_after_ms, Some(4));
    assert_eq!(snapshot.post_handler_ms, Some(5));
    assert_eq!(snapshot.total_duration_ms, Some(85));
}

#[tokio::test(start_paused = true)]
async fn dispatch_timing_measures_exec_spawn_from_request_acceptance() {
    let accepted_at = tokio::time::Instant::now();
    let timing = ToolDispatchTiming::new(accepted_at, /*eager*/ false);

    tokio::time::advance(std::time::Duration::from_millis(20)).await;
    timing.mark_first_poll();
    tokio::time::advance(std::time::Duration::from_millis(30)).await;
    timing.mark_exec_process_spawned();

    let live_snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(live_snapshot.item_to_first_poll_ms, Some(20));
    assert_eq!(live_snapshot.exec_request_to_spawn_ms, Some(50));
    assert!(live_snapshot.exec_process_alive_at_delivery);

    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    timing.mark_exec_process_exited();
    timing.record_outcome("failure");
    timing.record_exec_cleanup_state(
        /*background_process_expected*/ false, /*running_process_after_cleanup*/ true,
    );
    tokio::time::advance(std::time::Duration::from_millis(7)).await;

    let exited_snapshot = timing.snapshot(tokio::time::Instant::now());
    assert_eq!(exited_snapshot.exec_request_to_spawn_ms, Some(50));
    assert_eq!(exited_snapshot.exec_spawn_to_exit_ms, Some(100));
    assert_eq!(exited_snapshot.exec_exit_to_delivery_ms, Some(7));
    assert_eq!(exited_snapshot.exec_spawn_to_delivery_ms, Some(107));
    assert!(!exited_snapshot.exec_process_alive_at_delivery);
    assert_eq!(exited_snapshot.outcome, Some("failure"));
    assert!(exited_snapshot.exec_cleanup_state_observed);
    assert!(!exited_snapshot.exec_background_process_expected);
    assert!(exited_snapshot.exec_running_process_after_cleanup);
}

#[test]
fn relay_delivery_requires_enqueue_and_is_recorded_exactly_once() {
    let turn_timing = Arc::new(TurnTimingState::default());
    turn_timing.mark_turn_started();
    let timing =
        ToolDispatchTiming::new_with_turn_clock(turn_timing, tokio::time::Instant::now(), true);
    let execution_id = timing.execution_id().clone();
    let stale_execution_id = codex_protocol::protocol::ToolExecutionId("stale".to_string());

    assert!(!timing.mark_relay_delivery(&execution_id));
    assert!(timing.mark_relay_enqueue());
    assert!(!timing.mark_relay_delivery(&stale_execution_id));
    assert!(timing.mark_relay_delivery(&execution_id));
    assert!(!timing.mark_relay_delivery(&execution_id));

    assert_eq!(
        timing
            .snapshot(tokio::time::Instant::now())
            .lifecycle_events
            .iter()
            .filter(|event| event.boundary == ToolLifecycleBoundary::RelayDelivery)
            .count(),
        1
    );
}

struct TestHandler {
    tool_name: codex_tools::ToolName,
}

impl ToolExecutor<ToolInvocation> for TestHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: self.tool_name.name.clone(),
            description: "Test tool.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async {
            Ok(
                Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                    as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for TestHandler {}

#[tokio::test]
async fn dispatch_lifecycle_trace_records_direct_and_code_mode_requesters() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    attach_test_trace(&mut session, &turn, temp.path())?;
    session.services.rollout_thread_trace.start_code_cell_trace(
        turn.sub_id.as_str(),
        "cell-1",
        "call-code",
        "await tools.test_tool({})",
    );

    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
    }));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "direct-call",
                "test_tool",
                ToolCallSource::Direct,
                "{}",
            ),
            terminal_outcome_flag(),
        )
        .await?;
    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                session,
                turn,
                "code-mode-call",
                "test_tool",
                ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    parent_call_id: Some("outer-call".to_string()),
                    runtime_tool_call_id: "tool-1".to_string(),
                },
                "{}",
            ),
            terminal_outcome_flag(),
        )
        .await?;

    let replayed = codex_rollout_trace::replay_bundle(single_bundle_dir(temp.path())?)?;
    assert_eq!(
        replayed.tool_calls["direct-call"].model_visible_call_id,
        Some("direct-call".to_string()),
    );
    assert_eq!(
        replayed.tool_calls["direct-call"].requester,
        ToolCallRequester::Model,
    );
    assert!(
        replayed.tool_calls["direct-call"]
            .raw_invocation_payload_id
            .is_some(),
        "dispatch tracing should keep the tool invocation payload",
    );
    assert!(
        replayed.tool_calls["direct-call"]
            .raw_result_payload_id
            .is_some(),
        "direct calls should keep the model-facing result payload",
    );
    assert_eq!(
        replayed.tool_calls["code-mode-call"].model_visible_call_id,
        None,
    );
    assert_eq!(
        replayed.tool_calls["code-mode-call"].code_mode_runtime_tool_id,
        Some("tool-1".to_string()),
    );
    assert_eq!(
        replayed.tool_calls["code-mode-call"].requester,
        ToolCallRequester::CodeCell {
            code_cell_id: "code_cell:call-code".to_string(),
        },
    );
    assert!(
        replayed.tool_calls["code-mode-call"]
            .raw_result_payload_id
            .is_some(),
        "code-mode calls should keep the result returned to JavaScript",
    );

    Ok(())
}

#[tokio::test]
async fn dispatch_lifecycle_trace_records_unsupported_tool_failures() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    attach_test_trace(&mut session, &turn, temp.path())?;

    let registry = ToolRegistry::empty_for_test();
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                session,
                turn,
                "unsupported-call",
                "missing_tool",
                ToolCallSource::Direct,
                "{}",
            ),
            terminal_outcome_flag(),
        )
        .await;

    assert!(matches!(result, Err(FunctionCallError::RespondToModel(_))));
    let replayed = codex_rollout_trace::replay_bundle(single_bundle_dir(temp.path())?)?;
    let tool_call = &replayed.tool_calls["unsupported-call"];
    assert_eq!(tool_call.execution.status, ExecutionStatus::Failed);
    assert!(tool_call.raw_result_payload_id.is_some());

    Ok(())
}

#[tokio::test]
async fn dispatch_lifecycle_trace_records_incompatible_payload_failures() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    attach_test_trace(&mut session, &turn, temp.path())?;

    let registry = ToolRegistry::with_handler_for_test(Arc::new(TestHandler {
        tool_name: codex_tools::ToolName::plain("test_tool"),
    }));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let result = registry
        .dispatch_any_with_terminal_outcome(
            test_invocation_with_payload(
                session,
                turn,
                "incompatible-call",
                codex_tools::ToolName::plain("test_tool"),
                ToolCallSource::Direct,
                ToolPayload::Custom {
                    input: "{}".to_string(),
                },
            ),
            terminal_outcome_flag(),
        )
        .await;

    assert!(matches!(result, Err(FunctionCallError::Fatal(_))));
    let replayed = codex_rollout_trace::replay_bundle(single_bundle_dir(temp.path())?)?;
    let tool_call = &replayed.tool_calls["incompatible-call"];
    assert_eq!(tool_call.execution.status, ExecutionStatus::Failed);
    assert!(tool_call.raw_result_payload_id.is_some());

    Ok(())
}

#[tokio::test]
async fn missing_code_mode_wait_traces_only_the_wait_tool_call() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let (mut session, turn) = make_session_and_context().await;
    attach_test_trace(&mut session, &turn, temp.path())?;

    let registry = ToolRegistry::with_handler_for_test(Arc::new(CodeModeWaitHandler));
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    registry
        .dispatch_any_with_terminal_outcome(
            test_invocation(
                session,
                turn,
                "wait-call",
                WAIT_TOOL_NAME,
                ToolCallSource::Direct,
                r#"{"cell_id":"noop","terminate":true}"#,
            ),
            terminal_outcome_flag(),
        )
        .await?;

    let replayed = codex_rollout_trace::replay_bundle(single_bundle_dir(temp.path())?)?;
    assert_eq!(replayed.code_cells.len(), 0);
    assert!(
        replayed.tool_calls["wait-call"]
            .raw_result_payload_id
            .is_some()
    );

    Ok(())
}

fn test_invocation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: &str,
    tool_name: &str,
    source: ToolCallSource,
    arguments: &str,
) -> ToolInvocation {
    test_invocation_with_payload(
        session,
        turn,
        call_id,
        codex_tools::ToolName::plain(tool_name),
        source,
        ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    )
}

fn terminal_outcome_flag() -> Arc<std::sync::atomic::AtomicBool> {
    Arc::new(std::sync::atomic::AtomicBool::new(false))
}

fn test_invocation_with_payload(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: &str,
    tool_name: codex_tools::ToolName,
    source: ToolCallSource,
    payload: ToolPayload,
) -> ToolInvocation {
    let step_context = StepContext::for_test(Arc::clone(&turn));
    ToolInvocation {
        session,
        step_context,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name,
        source,
        payload,
    }
}

fn attach_test_trace(session: &mut Session, turn: &TurnContext, root: &Path) -> anyhow::Result<()> {
    let thread_id = session.thread_id;
    let rollout_thread_trace =
        codex_rollout_trace::ThreadTraceContext::start_root_in_root_for_test(
            root,
            ThreadStartedTraceMetadata {
                thread_id: thread_id.to_string(),
                agent_path: "/root".to_string(),
                task_name: None,
                nickname: None,
                agent_role: None,
                session_source: SessionSource::Exec,
                cwd: PathBuf::from("/workspace"),
                rollout_path: None,
                model: "gpt-test".to_string(),
                provider_name: "test-provider".to_string(),
                approval_policy: "never".to_string(),
                sandbox_policy: "danger-full-access".to_string(),
            },
        )?;
    rollout_thread_trace.record_codex_turn_started(turn.sub_id.as_str());
    session.services.rollout_thread_trace = rollout_thread_trace;
    Ok(())
}

fn single_bundle_dir(root: &Path) -> anyhow::Result<PathBuf> {
    let mut entries = fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    assert_eq!(entries.len(), 1);
    Ok(entries.remove(0))
}
