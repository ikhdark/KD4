use futures::FutureExt;
use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;

use crate::FunctionCallError;
use crate::session::InputQueueActivity;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::emit_failed_code_mode_cell_item;
use super::execute_handler::CellDispatchLease;
use super::handle_runtime_response;
use super::wait_spec::create_wait_tool;

// The runtime may spend two seconds draining already-issued notifications.
// Give it time to commit the terminal response and retained output afterward;
// matching that inner deadline races cleanup and discards recoverable evidence.
const INTERRUPTED_CELL_TERMINATION_GRACE: Duration = Duration::from_secs(5);
/// Compatibility default for bounded polls. Passive wait_for_output owns
/// continuation internally; cell waits wake on decisions, input, or the actor's idle bound.
pub(crate) const NESTED_DEFAULT_POLL: Duration = Duration::from_secs(285);

pub struct CodeModeWaitHandler;

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default, rename = "yield_time_ms")]
    _compatibility_yield_time_ms: Option<u64>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

#[derive(Debug)]
pub(super) enum OwnerHeldCodeModeExit {
    Runtime(codex_code_mode::WaitOutcome),
    InputActivity(InputQueueActivity),
}

#[derive(Debug)]
pub(super) struct OwnerHeldCodeModeWait {
    pub(super) exit: OwnerHeldCodeModeExit,
    pub(super) drained_observations: u32,
}

#[derive(Debug)]
pub(super) struct OwnerHeldCodeModeWaitError {
    pub(super) message: String,
    pub(super) drained_observations: u32,
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

impl ToolExecutor<ToolInvocation> for CodeModeWaitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_wait_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeWaitHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        match payload {
            ToolPayload::Function { arguments }
                if tool_name.namespace.is_none() && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments)?;
                let exec = ExecContext { session, turn };
                let started_at = Instant::now();
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                // An explicit request re-budgets the cell; a plain wait keeps
                // the budget the cell was started with (e.g. a raised exec
                // pragma) instead of resetting it to the default.
                exec.session.services.code_mode_service.record_output_budget(
                    &cell_id,
                    args.max_tokens.map(|max_tokens| max_tokens
                        .min(exec.turn.config.tool_output_token_limit
                            .unwrap_or(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL))
                        .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)),
                );
                let effective_max_tokens = exec
                    .session
                    .services
                    .code_mode_service
                    .output_budget(cell_id.as_str());
                let (wait_response, drained_observations) = if args.terminate {
                    exec.session
                        .services
                        .code_mode_service
                        .terminate(cell_id.clone())
                        .await
                        .map(|response| (response, 0))
                } else {
                    let turn_state = exec
                        .session
                        .input_queue
                        .turn_state_for_sub_id(&exec.session.active_turn, &exec.turn.sub_id)
                        .await;
                    let (activity_rx, _) = exec
                        .session
                        .input_queue
                        .subscribe_code_mode_activity(turn_state.as_deref(), false)
                        .await;
                    // Buffered output is not a new model decision. The script
                    // owns its awaited continuation until explicit yield or
                    // completion; input and the idle bound remain steerable.
                    let held = hold_until_state_change(
                        || {
                            exec.session
                                .services
                                .code_mode_service
                                .wait_for_decision(cell_id.clone())
                        },
                        &cancellation_token,
                        queued_input_activity(&exec, turn_state.as_deref(), activity_rx),
                        "wait cancelled",
                    )
                    .await;
                    let held = match held {
                        Ok(held) => held,
                        Err(error) if cancellation_token.is_cancelled() => OwnerHeldCodeModeWait {
                            exit: OwnerHeldCodeModeExit::Runtime(
                                terminate_interrupted_cell(&exec, &cell_id).await
                                    .map_err(FunctionCallError::RespondToModel)?,
                            ),
                            drained_observations: error.drained_observations,
                        },
                        Err(error) => {
                            record_internally_drained_waits(&exec, error.drained_observations);
                            return Err(FunctionCallError::RespondToModel(error.message));
                        }
                    };
                    let response = match held.exit {
                        OwnerHeldCodeModeExit::Runtime(response) => response,
                        OwnerHeldCodeModeExit::InputActivity(activity) => {
                            codex_code_mode::WaitOutcome::LiveCell(input_activity_response(
                                &cell_id, activity,
                            ))
                        }
                    };
                    Ok((response, held.drained_observations))
                }
                .map_err(FunctionCallError::RespondToModel)?;
                let authoritative_wait_signal =
                    terminal_wait_owner_signal(&wait_response, &cell_id);
                let keep_dispatch_open = matches!(
                    &wait_response,
                    codex_code_mode::WaitOutcome::LiveCell(
                        codex_code_mode::RuntimeResponse::Yielded { .. }
                            | codex_code_mode::RuntimeResponse::ExplicitYield { .. }
                    )
                );
                // Observation errors do not prove that the cell stopped. Only
                // retire a confirmed terminal response, after packet formatting.
                let dispatch_lease =
                    CellDispatchLease::new(Arc::clone(&exec.session), cell_id.clone());
                if keep_dispatch_open {
                    dispatch_lease.keep_open();
                }
                if matches!(&wait_response, codex_code_mode::WaitOutcome::LiveCell(_))
                    && let Some(parent_call_id) = exec
                        .session
                        .services
                        .code_mode_service
                        .cell_parent_call_id(&cell_id)
                {
                    // Nested calls the cell finished after its exec result reach
                    // the model through this wait's result.
                    exec.turn
                        .turn_timing_state
                        .record_code_mode_wait_delivery(&call_id, &parent_call_id);
                }
                let mut terminal_parent_call_id = None;
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response
                    && !matches!(
                        response,
                        codex_code_mode::RuntimeResponse::Yielded { .. }
                            | codex_code_mode::RuntimeResponse::ExplicitYield { .. }
                    )
                {
                    // Only a live-cell wait can close a CodeCell. A missing
                    // cell is still an ordinary `wait` tool result, but there
                    // is no runtime object for the reducer to complete.
                    let runtime_cell_id = match response {
                        codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::ExplicitYield { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                    };
                    terminal_parent_call_id = exec
                        .session
                        .services
                        .code_mode_service
                        .cell_parent_call_id(runtime_cell_id);
                    if exec.session.services.rollout_thread_trace.is_enabled() {
                        let trace = exec
                            .session
                            .services
                            .rollout_thread_trace
                            .code_cell_trace_context(
                                exec.turn.sub_id.as_str(),
                                runtime_cell_id.as_str(),
                            );
                        let response = response.clone();
                        dispatch_lease.record_trace(move || trace.record_ended(&response));
                    }
                }
                // The response is already captured. Cancellation releases only
                // this delivery gate, not the result or unrelated UI leases.
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {}
                    _ = exec.session.services.elicitations.wait_until_clear() => {}
                }
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response {
                    let owner = exec.session.services.code_mode_service.cell_parent_call_id(&cell_id);
                    let parent_call_id =
                        failed_cell_owner_call_id(owner.as_deref().or(terminal_parent_call_id.as_deref()), &call_id);
                    emit_failed_code_mode_cell_item(&exec, parent_call_id, response, started_at)
                        .await;
                }
                let response = wait_response.into();
                exec.session.services.code_mode_service.flush_packet_retention(&cell_id).await;
                let delivery = exec.session.services.code_mode_service.delivery_for_response(
                    &cell_id, &exec.turn, &response,
                );
                let mut output = handle_runtime_response(
                    &exec,
                    response,
                    effective_max_tokens,
                    started_at,
                )
                .map_err(FunctionCallError::RespondToModel)?;
                output =
                    attach_drained_wait_evidence(&exec, output, &cell_id, drained_observations);
                if let Some(signal) = authoritative_wait_signal {
                    output = super::merge_code_mode_signal(output, signal);
                }
                super::execute_handler::attach_delivery_decision(&mut output, delivery);
                Ok(boxed_tool_output(output))
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        }
    }
}

fn failed_cell_owner_call_id<'a>(
    original_exec_call_id: Option<&'a str>,
    wait_call_id: &'a str,
) -> &'a str {
    original_exec_call_id.unwrap_or(wait_call_id)
}

pub(super) async fn terminate_interrupted_cell(
    exec: &ExecContext,
    cell_id: &codex_code_mode::CellId,
) -> Result<codex_code_mode::WaitOutcome, String> {
    let termination = tokio::time::timeout(
        INTERRUPTED_CELL_TERMINATION_GRACE,
        exec.session
            .services
            .code_mode_service
            .terminate(cell_id.clone()),
    )
    .await;
    match termination {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => {
            warn!(
                turn_id = %exec.turn.sub_id,
                runtime_cell_id = %cell_id,
                %error,
                "failed to terminate interrupted code mode cell"
            );
            Err(error)
        }
        Err(_) => {
            warn!(
                turn_id = %exec.turn.sub_id,
                runtime_cell_id = %cell_id,
                grace_ms = INTERRUPTED_CELL_TERMINATION_GRACE.as_millis(),
                "timed out terminating interrupted code mode cell"
            );
            Err(format!("code-mode cell {cell_id} cancellation cleanup timed out; effects and buffered output are unknown"))
        }
    }
}

fn terminal_wait_owner_signal(
    outcome: &codex_code_mode::WaitOutcome,
    cell_id: &codex_code_mode::CellId,
) -> Option<serde_json::Value> {
    let response = match outcome {
        codex_code_mode::WaitOutcome::LiveCell(response)
        | codex_code_mode::WaitOutcome::MissingCell(response) => response,
    };
    let state = match response {
        codex_code_mode::RuntimeResponse::Terminated { .. } => "terminated",
        codex_code_mode::RuntimeResponse::Result { error_text, .. } => {
            if error_text.is_some() {
                "failed"
            } else {
                "completed"
            }
        }
        codex_code_mode::RuntimeResponse::Yielded { .. }
        | codex_code_mode::RuntimeResponse::ExplicitYield { .. } => return None,
    };
    Some(serde_json::json!({
        "authoritative_wait_owner_v1": {
            "adapter": "code_mode_cell",
            "disposition": "terminal",
            "owner": cell_id.as_str(),
            "state_revision": state,
        }
    }))
}

pub(super) async fn hold_until_state_change<F, Fut>(
    wait_once: F,
    cancellation_token: &tokio_util::sync::CancellationToken,
    input_activity: impl Future<Output = InputQueueActivity>,
    cancellation_message: &'static str,
) -> Result<OwnerHeldCodeModeWait, OwnerHeldCodeModeWaitError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<codex_code_mode::WaitOutcome, String>>,
{
    let cancelled = || OwnerHeldCodeModeWaitError {
        message: cancellation_message.to_string(),
        drained_observations: 0,
    };
    let steered = |activity| OwnerHeldCodeModeWait {
        exit: OwnerHeldCodeModeExit::InputActivity(activity),
        drained_observations: 0,
    };
    if cancellation_token.is_cancelled() {
        return Err(cancelled());
    }
    tokio::pin!(input_activity);
    // The runtime retains a decision only while no observer holds it; one
    // delivered to an observer that is then dropped is lost. Answer input that
    // is already waiting before starting an observation.
    if let Some(activity) = input_activity.as_mut().now_or_never() {
        return Ok(steered(activity));
    }
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(cancelled()),
        // A decision delivered in the same wakeup as new input wins; the input
        // stays queued for the next sampling request.
        result = wait_once() => {
            result
                .map(|response| OwnerHeldCodeModeWait {
                    exit: OwnerHeldCodeModeExit::Runtime(response),
                    drained_observations: 0,
                })
                .map_err(|message| OwnerHeldCodeModeWaitError {
                    message,
                    drained_observations: 0,
                })
        }
        activity = &mut input_activity => {
            Ok(steered(activity))
        }
    }
}

pub(super) async fn queued_input_activity(
    exec: &ExecContext,
    turn_state: Option<&tokio::sync::Mutex<crate::state::TurnState>>,
    mut activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
) -> InputQueueActivity {
    loop {
        // Mark the wake seen before reading retained state: anything arriving
        // during the read is either in that state or leaves another wake.
        let internal_completion = *activity_rx.borrow_and_update() == InputQueueActivity::InternalCompletion;
        if let Some(activity) = exec.session.input_queue
            .pending_activity(turn_state, internal_completion).await
        {
            return activity;
        }
        if activity_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
async fn next_input_activity(
    mut activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
    mut pending_activity: Option<InputQueueActivity>,
) -> InputQueueActivity {
    if let Some(activity) = pending_activity.take() {
        return activity;
    }
    loop {
        if activity_rx.changed().await.is_ok() {
            return *activity_rx.borrow_and_update();
        }
        std::future::pending::<()>().await;
    }
}

fn unchanged_wait_receipt(
    cell_id: &codex_code_mode::CellId,
    suppressed_continuation_count: u32,
) -> TurnTimingDeterministicContinuationReceipt {
    let resource_identity_hash = format!(
        "{:x}",
        Sha256::digest(format!("code-mode-cell\0{}", cell_id.as_str()).as_bytes())
    );
    TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::UnchangedWait,
        wire_identity: String::new(),
        resource_identity_hash,
        // Cell ids are allocated from a never-reused lifecycle namespace by
        // the code-mode owner. Pair that lifecycle identity with its running
        // phase instead of reporting one constant revision for every cell.
        state_revision: format!(
            "{:x}",
            Sha256::digest(format!(
                "code-mode-cell-lifecycle\0{}\0running",
                cell_id.as_str()
            ))
        ),
        host_action: DeterministicContinuationHostAction::AwaitStateChange,
        action_bounds_hash: format!(
            "{:x}",
            Sha256::digest(b"operation-lifetime:cell-terminal-or-turn-cancellation")
        ),
        suppressed_continuation_count,
    }
}

pub(super) fn input_activity_response(
    cell_id: &codex_code_mode::CellId,
    activity: InputQueueActivity,
) -> codex_code_mode::RuntimeResponse {
    let text = match activity {
        InputQueueActivity::Mailbox => "Wait interrupted by mailbox activity.",
        InputQueueActivity::Steer => "Wait interrupted by new user input.",
        InputQueueActivity::InternalCompletion => {
            "A deferred MCP result is waiting in your next response. Read it instead of polling."
        }
    };
    codex_code_mode::RuntimeResponse::Yielded {
        cell_id: cell_id.clone(),
        content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
            text: text.to_string(),
        }],
    }
}

pub(super) fn record_internally_drained_waits(exec: &ExecContext, count: u32) {
    exec.turn
        .turn_timing_state
        .record_internally_drained_waits(count);
}

pub(super) fn attach_drained_wait_evidence(
    exec: &ExecContext,
    mut output: FunctionToolOutput,
    cell_id: &codex_code_mode::CellId,
    drained_observations: u32,
) -> FunctionToolOutput {
    record_internally_drained_waits(exec, drained_observations);
    if drained_observations > 0 {
        output = output.with_deterministic_continuation_receipt(unchanged_wait_receipt(
            cell_id,
            drained_observations,
        ));
    }
    output
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn delegates_workspace_admission(&self) -> bool {
        true
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        // Cancellation must keep polling the handler through bounded cell
        // termination so the V8/runtime owner cannot be orphaned.
        true
    }

    fn pre_tool_use_hook_name(
        &self,
        _tool_name: &codex_tools::ToolName,
        _payload: &crate::tools::context::ToolPayload,
    ) -> Option<HookToolName> {
        None
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself.
        None
    }

    fn post_tool_use_hook_name(&self, _invocation: &ToolInvocation) -> Option<HookToolName> {
        None
    }

    fn post_tool_use_payload(
        &self,
        _invocation: &ToolInvocation,
        _result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // The wait result feeds code-mode control flow, so do not let
        // PostToolUse replace it with model-facing hook feedback.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ActiveTurn;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn explicit_empty_yield(cell_id: &codex_code_mode::CellId) -> codex_code_mode::WaitOutcome {
        codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::ExplicitYield {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
        })
    }

    #[tokio::test]
    async fn terminal_wait_preserves_required_outcome_and_semantic_evidence() {
        use crate::tools::context::RequiredToolTerminalCause;
        for (cause, expected_outcome) in [
            (RequiredToolTerminalCause::Failure, "failure"),
            (RequiredToolTerminalCause::Blocked, "blocked"),
        ] {
            let (mut session, turn) = crate::session::tests::make_session_and_context().await;
            session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
                codex_code_mode::InProcessCodeModeSessionProvider,
            ));
            let session = Arc::new(session);
            let turn = Arc::new(turn);
            let service = &session.services.code_mode_service;
            let started = service
                .execute(codex_code_mode::ExecuteRequest {
                    state_path: None,
                    tool_call_id: "outer-exec".to_string(),
                    enabled_tools: Vec::new().into(),
                    source: "await yield_control();".to_string(),
                    yield_time_ms: None,
                    max_output_tokens: None,
                    default_tool_timeout_ms: None,
                })
                .await
                .unwrap();
            let cell = started.cell_id.clone();
            service.record_cell_parent_call_id(&cell, "outer-exec");
            service.mark_cell_ready_for_dispatch(&cell);
            assert!(matches!(
                started.initial_response().await.unwrap(),
                codex_code_mode::RuntimeResponse::ExplicitYield { .. }
            ));
            let ordinal = service.begin_packet_call(&cell).unwrap();
            service.complete_packet_call(
                &cell,
                ordinal,
                false,
                0,
                Vec::new(),
                None,
                Some((cause, "REQUIRED_DIAGNOSTIC".to_string())),
            );
            let output = CodeModeWaitHandler
                .handle_call(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: crate::session::step_context::StepContext::for_test(turn),
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    call_id: "wait-call".to_string(),
                    tool_name: ToolName::plain(WAIT_TOOL_NAME),
                    source: crate::tools::router::ToolCallSource::Direct,
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({ "cell_id": cell.as_str() }).to_string(),
                    },
                })
                .await
                .unwrap();
            let signal = output.sampling_request_signal().unwrap();
            assert_eq!(signal["outcome"], expected_outcome);
            assert_eq!(
                signal["authoritative_wait_owner_v1"]["state_revision"],
                "completed"
            );
            assert_eq!(
                signal["authoritative_wait_owner_v1"]["owner"],
                cell.as_str()
            );
            assert!(
                signal["semantic_evidence"]
                    .to_string()
                    .contains("REQUIRED_DIAGNOSTIC")
            );
            assert!(
                signal["failure_signature"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
            assert!(output.log_preview().contains("REQUIRED_DIAGNOSTIC"));
            assert!(
                !service
                    .packet_admission
                    .lock()
                    .unwrap()
                    .cells
                    .contains_key(cell.as_str())
            );
            service.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn wait_result_attests_nested_calls_the_cell_finished() {
        use crate::tools::tool_dispatch_trace::ToolDispatchTimingSnapshot;
        use crate::turn_timing::ToolCallTimingLineage;
        use codex_protocol::protocol::ToolExecutionId;
        use codex_protocol::protocol::TurnTimingToolCallSource;

        let (mut session, turn) = crate::session::tests::make_session_and_context().await;
        session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
            codex_code_mode::InProcessCodeModeSessionProvider,
        ));
        let session = Arc::new(session);
        let timing = Arc::clone(&turn.turn_timing_state);
        let turn = Arc::new(turn);
        let service = &session.services.code_mode_service;
        let started = service
            .execute(codex_code_mode::ExecuteRequest {
                state_path: None,
                tool_call_id: "outer-exec".to_string(),
                enabled_tools: Vec::new().into(),
                source: "await yield_control();".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            })
            .await
            .unwrap();
        let cell = started.cell_id.clone();
        service.record_cell_parent_call_id(&cell, "outer-exec");
        service.mark_cell_ready_for_dispatch(&cell);
        assert!(matches!(
            started.initial_response().await.unwrap(),
            codex_code_mode::RuntimeResponse::ExplicitYield { .. }
        ));
        // The yielded cell finished a nested call after its exec result, so
        // only this wait's result carries it to the model.
        let wait_execution = ToolExecutionId("wait-execution".to_string());
        timing.record_accepted_tool_call(
            "wait-call",
            &wait_execution,
            TurnTimingToolCallSource::Direct,
            None,
        );
        let nested_execution = ToolExecutionId("nested-execution".to_string());
        timing.record_accepted_tool_call(
            "exec-1-tool-1",
            &nested_execution,
            TurnTimingToolCallSource::CodeMode,
            Some("outer-exec"),
        );
        timing.record_tool_dispatch_timing(
            "exec-1-tool-1",
            "write_stdin",
            TurnTimingToolCallSource::CodeMode,
            ToolCallTimingLineage {
                parent_call_id: Some("outer-exec"),
                ..ToolCallTimingLineage::default()
            },
            ToolDispatchTimingSnapshot {
                execution_id: nested_execution,
                outcome: Some("success"),
                ..ToolDispatchTimingSnapshot::default()
            },
        );

        CodeModeWaitHandler
            .handle_call(ToolInvocation {
                session: Arc::clone(&session),
                step_context: crate::session::step_context::StepContext::for_test(turn),
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(tokio::sync::Mutex::new(
                    crate::turn_diff_tracker::TurnDiffTracker::new(),
                )),
                call_id: "wait-call".to_string(),
                tool_name: ToolName::plain(WAIT_TOOL_NAME),
                source: crate::tools::router::ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({ "cell_id": cell.as_str() }).to_string(),
                },
            })
            .await
            .unwrap();
        timing.record_tool_result_persisted("wait-call");

        let closure = timing.tool_closure_snapshot();
        assert_eq!(closure.persisted_count, 2);
        assert!(
            closure
                .unresolved_calls
                .iter()
                .all(|call| call.call_id != "exec-1-tool-1")
        );
        service.shutdown().await.unwrap();
    }

    #[test]
    fn terminal_signal_is_private_to_final_typed_cell_states() {
        let cell_id = codex_code_mode::CellId::new("cell-terminal".to_string());
        let terminal =
            codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
                error_text: None,
            });
        assert_eq!(
            terminal_wait_owner_signal(&terminal, &cell_id).and_then(|signal| signal
                .pointer("/authoritative_wait_owner_v1/disposition")
                .cloned()),
            Some(serde_json::json!("terminal"))
        );
        assert_eq!(
            terminal_wait_owner_signal(&terminal, &cell_id).and_then(|signal| signal
                .pointer("/authoritative_wait_owner_v1/surfaceable_message")
                .cloned()),
            None,
            "raw code-mode output has no owner-designated completion projection"
        );
        assert!(terminal_wait_owner_signal(&explicit_empty_yield(&cell_id), &cell_id).is_none());
    }

    #[test]
    fn failed_waited_cell_remains_owned_by_the_original_exec_call() {
        assert_eq!(
            failed_cell_owner_call_id(Some("outer-exec"), "synthetic-wait"),
            "outer-exec"
        );
        assert_eq!(
            failed_cell_owner_call_id(None, "standalone-wait"),
            "standalone-wait"
        );
    }

    #[tokio::test]
    async fn explicit_empty_state_change_is_returned_without_resampling() {
        let cell_id = codex_code_mode::CellId::new("cell-1".to_string());
        let mut observations = 0_u32;

        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let held = hold_until_state_change(
            || {
                observations = observations.saturating_add(1);
                std::future::ready(Ok(explicit_empty_yield(&cell_id)))
            },
            &tokio_util::sync::CancellationToken::new(),
            next_input_activity(activity_rx, None),
            "wait cancelled",
        )
        .await
        .expect("held wait");

        assert!(matches!(
            held.exit,
            OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::ExplicitYield { content_items, .. }
            )) if content_items.is_empty()
        ));
        assert_eq!(held.drained_observations, 0);
        assert_eq!(observations, 1);
    }

    #[tokio::test]
    async fn held_wait_preserves_error_and_cancellation() {
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let error = hold_until_state_change(
            || std::future::ready(Err("runtime failed".to_string())),
            &tokio_util::sync::CancellationToken::new(),
            next_input_activity(activity_rx, None),
            "wait cancelled",
        )
        .await;
        assert!(matches!(
            error,
            Err(OwnerHeldCodeModeWaitError {
                message,
                drained_observations: 0,
            })
                if message == "runtime failed"
        ));

        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let cancelled = hold_until_state_change(
            std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
            &cancellation,
            next_input_activity(activity_rx, None),
            "wait cancelled",
        )
        .await;
        assert!(matches!(
            cancelled,
            Err(OwnerHeldCodeModeWaitError {
                message,
                drained_observations: 0,
            })
                if message == "wait cancelled"
        ));
    }

    #[tokio::test]
    async fn steering_wakes_and_detaches_a_suspended_owner_observer() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_for_wait = Arc::clone(&dropped);

        let held = tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            hold_until_state_change(
                move || {
                    let started_tx = started_tx.take();
                    let marker = DropMarker(Arc::clone(&dropped_for_wait));
                    async move {
                        let _marker = marker;
                        if let Some(started_tx) = started_tx {
                            let _ = started_tx.send(());
                        }
                        std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>().await
                    }
                },
                &cancellation,
                next_input_activity(activity_rx, None),
                "wait cancelled",
            )
            .await
        });

        started_rx.await.expect("observer started");
        activity_tx.send_replace(InputQueueActivity::Steer);
        let result = tokio::time::timeout(Duration::from_secs(1), held)
            .await
            .expect("steering should wake immediately")
            .expect("held wait task")
            .expect("steering is non-terminal");

        assert!(matches!(
            result.exit,
            OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Steer)
        ));
        assert_eq!(result.drained_observations, 0);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn held_wait_ignores_non_triggering_mail_until_triggering_mail_arrives() {
        for queued_before_subscription in [true, false] {
            let (session, turn) = crate::session::tests::make_session_and_context().await;
            let exec = ExecContext { session: Arc::new(session), turn: Arc::new(turn) };
            let queue = &exec.session.input_queue;
            let mail = |trigger_turn| codex_protocol::protocol::InterAgentCommunication::new(
                codex_protocol::AgentPath::root().join("worker").unwrap(),
                codex_protocol::AgentPath::root(),
                Vec::new(),
                "worker update".to_string(),
                trigger_turn,
            );
            if queued_before_subscription {
                queue.enqueue_mailbox_communication(mail(false)).await.unwrap();
            }
            let (activity_rx, pending_activity) = queue.subscribe_code_mode_activity(None, false).await;
            assert_eq!(pending_activity, None);
            let cancellation = tokio_util::sync::CancellationToken::new();
            let mut held = Box::pin(hold_until_state_change(
                std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
                &cancellation,
                queued_input_activity(&exec, None, activity_rx),
                "wait cancelled",
            ));
            assert!(held.as_mut().now_or_never().is_none());
            if !queued_before_subscription {
                queue.enqueue_mailbox_communication(mail(false)).await.unwrap();
            }
            assert!(held.as_mut().now_or_never().is_none());
            assert!(queue.has_pending_mailbox_items().await);
            queue.enqueue_mailbox_communication(mail(true)).await.unwrap();
            let result = held.await.expect("triggering mail wakes the wait");
            assert!(matches!(result.exit, OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Mailbox)));
            assert_eq!(queue.get_pending_input(&tokio::sync::Mutex::new(None)).await.len(), 2);
        }
    }

    #[tokio::test]
    async fn held_wait_rechecks_stale_wakes_and_prioritizes_queued_steering() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let exec = ExecContext { session: Arc::new(session), turn: Arc::new(turn) };
        let queue = &exec.session.input_queue;
        let (receiver, _) = queue.subscribe_code_mode_activity(None, false).await;
        let mail = codex_protocol::protocol::InterAgentCommunication::new(
            codex_protocol::AgentPath::root().join("worker").unwrap(),
            codex_protocol::AgentPath::root(), Vec::new(), "update".into(), true,
        );
        queue.enqueue_mailbox_communication(mail.clone()).await.unwrap();
        queue.get_pending_input(&tokio::sync::Mutex::new(None)).await;
        let mut pending = Box::pin(queued_input_activity(&exec, None, receiver));
        assert!(pending.as_mut().now_or_never().is_none(), "a drained mailbox wake is stale");
        queue.restore_transferred_startup_input(vec![crate::session::TurnInput::UserInput {
            content: vec![codex_protocol::user_input::UserInput::Text { text: "steer".into(), text_elements: Vec::new() }],
            client_id: None,
        }]).await;
        // A lower-priority broadcast must not mask the retained user input.
        queue.publish_internal_completion();
        assert_eq!(pending.await, InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn decision_delivered_with_new_input_is_not_dropped() {
        let cell_id = codex_code_mode::CellId::new("cell-1".to_string());
        let (activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
        let held = tokio::spawn(async move {
            hold_until_state_change(
                move || async move {
                    let _ = started_tx.send(());
                    decision_rx
                        .await
                        .map_err(|_| "decision dropped".to_string())
                },
                &tokio_util::sync::CancellationToken::new(),
                next_input_activity(activity_rx, None),
                "wait cancelled",
            )
            .await
        });

        started_rx.await.expect("observer started");
        // The cell yields in the same wakeup as the user steers.
        decision_tx
            .send(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::ExplicitYield {
                    cell_id: cell_id.clone(),
                    content_items: vec![
                        codex_code_mode::FunctionCallOutputContentItem::InputText {
                            text: "yielded output".to_string(),
                        },
                    ],
                },
            ))
            .expect("observer is waiting");
        activity_tx.send_replace(InputQueueActivity::Steer);

        let held = held
            .await
            .expect("held wait task")
            .expect("a decision is non-terminal");
        assert!(matches!(
            held.exit,
            OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::ExplicitYield { content_items, .. }
            )) if matches!(
                content_items.as_slice(),
                [codex_code_mode::FunctionCallOutputContentItem::InputText { text }]
                    if text == "yielded output"
            )
        ));
    }

    #[tokio::test]
    async fn waiting_input_returns_before_an_observation_starts() {
        for queued_before_subscription in [true, false] {
            let (activity_tx, activity_rx) =
                tokio::sync::watch::channel(InputQueueActivity::Mailbox);
            let pending_activity = if queued_before_subscription {
                Some(InputQueueActivity::Steer)
            } else {
                activity_tx.send_replace(InputQueueActivity::Steer);
                None
            };
            let observed = Arc::new(AtomicBool::new(false));
            let observed_by_wait = Arc::clone(&observed);
            let held = hold_until_state_change(
                move || async move {
                    observed_by_wait.store(true, Ordering::Release);
                    std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>().await
                },
                &tokio_util::sync::CancellationToken::new(),
                next_input_activity(activity_rx, pending_activity),
                "wait cancelled",
            )
            .await
            .expect("waiting input is non-terminal");

            assert!(matches!(
                held.exit,
                OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Steer)
            ));
            assert!(
                !observed.load(Ordering::Acquire),
                "an observation abandoned for input can lose a delivered decision"
            );
        }
    }

    #[tokio::test]
    async fn injected_response_item_wakes_a_suspended_owner_observer() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let exec = ExecContext { session: Arc::new(session), turn: Arc::new(turn) };
        let session = Arc::clone(&exec.session);
        let turn_state = {
            let mut active_turn = session.active_turn.lock().await;
            Arc::clone(
                &active_turn
                    .get_or_insert_with(ActiveTurn::default)
                    .turn_state,
            )
        };
        let (activity_rx, pending_activity) = session
            .input_queue
            .subscribe_code_mode_activity(
                Some(turn_state.as_ref()),
                /*has_internal_completion*/ false,
            )
            .await;
        assert_eq!(pending_activity, None);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let held = tokio::spawn(async move {
            let cancellation = tokio_util::sync::CancellationToken::new();
            hold_until_state_change(
                move || async move {
                    let _ = started_tx.send(());
                    std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>().await
                },
                &cancellation,
                queued_input_activity(&exec, Some(turn_state.as_ref()), activity_rx),
                "wait cancelled",
            )
            .await
        });

        started_rx.await.expect("observer started");
        session
            .inject_if_running(vec![ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "The active goal objective was updated.".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .expect("active-turn injection should succeed");

        let result = tokio::time::timeout(Duration::from_secs(1), held)
            .await
            .expect("injected response item should wake immediately")
            .expect("held wait task")
            .expect("injected response item is non-terminal");
        assert!(matches!(
            result.exit,
            OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Steer)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn held_wait_ignores_silence_but_wakes_for_input() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let held = tokio::spawn(async move {
            hold_until_state_change(
                std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
                &cancellation,
                next_input_activity(activity_rx, None),
                "wait cancelled",
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(!held.is_finished());

        tokio::time::advance(Duration::from_secs(600)).await;
        tokio::task::yield_now().await;
        assert!(!held.is_finished());
        activity_tx.send(InputQueueActivity::Steer).unwrap();
        let held = held
            .await
            .expect("held wait task")
            .expect("input wakes the held wait");
        assert!(matches!(held.exit, OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Steer)));
        assert_eq!(held.drained_observations, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn real_code_mode_wait_survives_silence_until_steered() {
        // Runtime initialization runs on a native thread, outside Tokio's paused clock.
        tokio::time::resume();
        let service = Arc::new(crate::tools::code_mode::CodeModeService::new(Arc::new(
            codex_code_mode::InProcessCodeModeSessionProvider,
        )));
        let started_cell = service
            .execute(codex_code_mode::ExecuteRequest {
                state_path: None,
                tool_call_id: "runtime-timeout-call".to_string(),
                enabled_tools: Vec::new().into(),
                source: "await new Promise(resolve => setTimeout(resolve, 3_600_000));".to_string(),
                yield_time_ms: Some(1),
                max_output_tokens: None,
                default_tool_timeout_ms: None,
            })
            .await
            .expect("real code-mode cell should start");
        let cell_id = started_cell.cell_id.clone();
        assert!(matches!(
            started_cell
                .initial_response()
                .await
                .expect("real code-mode cell should reach its initial yield"),
            codex_code_mode::RuntimeResponse::Yielded {
                content_items,
                ..
            } if content_items.is_empty()
        ));

        tokio::time::pause();
        let wait_service = Arc::clone(&service);
        let wait_cell_id = cell_id.clone();
        let (activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let held = tokio::spawn(async move {
            let cancellation = tokio_util::sync::CancellationToken::new();
            hold_until_state_change(
                move || {
                    let service = Arc::clone(&wait_service);
                    async move { service.wait_for_decision(wait_cell_id).await }
                },
                &cancellation,
                next_input_activity(activity_rx, None),
                "wait cancelled",
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(599)).await;
        tokio::task::yield_now().await;
        assert!(!held.is_finished());
        activity_tx.send(InputQueueActivity::Steer).unwrap();
        let held = held
            .await
            .expect("real state-change wait task")
            .expect("real state-change wait should wake on steering");
        assert!(matches!(held.exit, OwnerHeldCodeModeExit::InputActivity(InputQueueActivity::Steer)));

        // Steering released the wait without touching the cell: it is still
        // live, so explicit termination is what ends it.
        assert!(matches!(
            service
                .terminate(cell_id)
                .await
                .expect("idle real cell should still be terminable"),
            codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::Terminated { .. }
            )
        ));
        service
            .shutdown()
            .await
            .expect("real code-mode runtime should shut down");
    }

    #[test]
    fn unchanged_wait_revision_is_cell_lifecycle_specific() {
        let first = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-a".to_string()), 1);
        let same = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-a".to_string()), 2);
        let other = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-b".to_string()), 1);

        assert_eq!(first.state_revision, same.state_revision);
        assert_ne!(first.state_revision, other.state_revision);
    }
}
