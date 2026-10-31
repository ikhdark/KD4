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
// Bounds the detached termination observer for runtimes without their own
// deadline. It must outlast the remote host's ten-second terminate deadline:
// that request settles itself, and dropping it while the host still owes an
// acknowledgment fails the shared host connection closed for every thread.
const DETACHED_TERMINATION_OBSERVATION_LIMIT: Duration = Duration::from_secs(60);
// Most cells preempted by already-queued mail finish well inside a second,
// which is far less than the model request needed to wait for them again.
const MAILBOX_PREEMPTION_GRACE: Duration = Duration::from_secs(1);
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
                let context_token_limit = step_context.model_output_token_limit(
                    &session,
                    codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
                ).await;
                let exec = ExecContext { session, turn };
                let started_at = Instant::now();
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                // An explicit request re-budgets the cell; a plain wait keeps
                // the budget the cell was started with (e.g. a raised exec
                // pragma) instead of resetting it to the default.
                exec.session
                    .services
                    .code_mode_service
                    .record_output_budget(
                        &cell_id,
                        args.max_tokens.map(|max_tokens| {
                            max_tokens
                                .min(
                                    exec.turn.config.tool_output_token_limit.unwrap_or(
                                        codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
                                    ),
                                )
                                .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
                        }),
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
                        queued_input_activity(&exec, turn_state.as_deref(), activity_rx.clone()),
                        queued_steering_activity(&exec, turn_state.as_deref(), activity_rx),
                        "wait cancelled",
                    )
                    .await;
                    let held = match held {
                        Ok(held) => held,
                        Err(error) if cancellation_token.is_cancelled() => OwnerHeldCodeModeWait {
                            exit: OwnerHeldCodeModeExit::Runtime(
                                terminate_interrupted_cell(&exec, &cell_id)
                                    .await
                                    .unwrap_or_else(|error| codex_code_mode::WaitOutcome::LiveCell(super::observation_error_response(&cell_id, error))),
                            ),
                            drained_observations: error.drained_observations,
                        },
                        Err(error) => {
                            OwnerHeldCodeModeWait {
                                exit: OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                                    super::observation_error_response(&cell_id, error.message))),
                                drained_observations: error.drained_observations,
                            }
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
                .unwrap_or_else(|error| (codex_code_mode::WaitOutcome::LiveCell(super::observation_error_response(&cell_id, error)), 0));
                let authoritative_wait_signal =
                    terminal_wait_owner_signal(&wait_response, &cell_id);
                let keep_dispatch_open = matches!(
                    &wait_response,
                    codex_code_mode::WaitOutcome::LiveCell(response)
                        if !super::runtime_response_is_terminal(response)
                );
                // Observation errors do not prove that the cell stopped. Only
                // retire a confirmed terminal response, after packet formatting.
                let dispatch_lease =
                    CellDispatchLease::new(Arc::clone(&exec.session), cell_id.clone());
                if keep_dispatch_open {
                    dispatch_lease.keep_open();
                }
                let mut terminal_parent_call_id = None;
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response
                    && super::runtime_response_is_terminal(response)
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
                }
                // The response is already captured. Cancellation releases only
                // this delivery gate, not the result or unrelated UI leases.
                let dialog_wait_started = Instant::now();
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {}
                    _ = exec.session.services.elicitations.wait_until_clear() => {}
                }
                tracing::info!(event.name = "codex.code_mode.dialog_delivery_gate",
                    turn_id = %exec.turn.sub_id, runtime_cell_id = %cell_id,
                    tool_call_id = %call_id,
                    dialog_wait_ms = u64::try_from(dialog_wait_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    cancelled = cancellation_token.is_cancelled(),
                    scope = "session_wide", "captured cell response finished dialog delivery gate");
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response {
                    let owner = exec
                        .session
                        .services
                        .code_mode_service
                        .cell_parent_call_id(&cell_id);
                    let parent_call_id = failed_cell_owner_call_id(
                        owner.as_deref().or(terminal_parent_call_id.as_deref()),
                        &call_id,
                    );
                    emit_failed_code_mode_cell_item(&exec, parent_call_id, response, started_at)
                        .await;
                }
                let response = wait_response.into();
                let retention_settled = exec
                    .session
                    .services
                    .code_mode_service
                    .observe_packet_retention(&cell_id, args.terminate || super::observation_failed(&response), &cancellation_token)
                    .await;
                let (mut output, delivered_call_ids, delivery) =
                    handle_runtime_response(&exec, response, effective_max_tokens, context_token_limit, started_at)
                        .map_err(FunctionCallError::RespondToModel)?;
                if !retention_settled {
                    super::attach_pending_retention(&mut output, &cell_id, &call_id);
                }
                output =
                    attach_drained_wait_evidence(&exec, output, &cell_id, drained_observations);
                if let Some(signal) = authoritative_wait_signal {
                    output = super::merge_code_mode_signal(output, signal);
                }
                super::execute_handler::attach_delivery_decision(&mut output, delivery);
                exec.turn
                    .turn_timing_state
                    .record_code_mode_projection_delivery(&call_id, &delivered_call_ids);
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
    // The grace bounds only how long this caller observes termination. The
    // request runs detached: dropping an accepted remote terminate marks the
    // shared host connection dead, so a termination finishing after the grace
    // would otherwise replace the host and its stored values for every thread.
    let session = Arc::clone(&exec.session);
    let detached_cell_id = cell_id.clone();
    let mut termination = tokio::spawn(async move {
        tokio::time::timeout(
            DETACHED_TERMINATION_OBSERVATION_LIMIT,
            session
                .services
                .code_mode_service
                .terminate(detached_cell_id),
        )
        .await
        .unwrap_or_else(|_| Err("code-mode cell termination observation expired".to_string()))
    });
    match tokio::time::timeout(INTERRUPTED_CELL_TERMINATION_GRACE, &mut termination).await {
        Ok(Ok(Ok(response))) => Ok(response),
        Ok(Ok(Err(error))) => {
            warn!(
                turn_id = %exec.turn.sub_id,
                runtime_cell_id = %cell_id,
                %error,
                "failed to terminate interrupted code mode cell"
            );
            Err(error)
        }
        Ok(Err(join_error)) => {
            warn!(
                turn_id = %exec.turn.sub_id,
                runtime_cell_id = %cell_id,
                error = %join_error,
                "interrupted code mode cell termination task failed"
            );
            Err(format!(
                "code-mode cell {cell_id} cancellation cleanup failed; effects and buffered output are unknown"
            ))
        }
        Err(_) => {
            warn!(
                turn_id = %exec.turn.sub_id,
                runtime_cell_id = %cell_id,
                grace_ms = INTERRUPTED_CELL_TERMINATION_GRACE.as_millis(),
                "timed out terminating interrupted code mode cell"
            );
            Err(format!(
                "code-mode cell {cell_id} cancellation cleanup timed out; effects and buffered output are unknown"
            ))
        }
    }
}

fn terminal_wait_owner_signal(
    outcome: &codex_code_mode::WaitOutcome,
    cell_id: &codex_code_mode::CellId,
) -> Option<serde_json::Value> {
    let response = match outcome {
        codex_code_mode::WaitOutcome::LiveCell(response) => response,
        // Missing lifecycle evidence reports uncertainty, not a terminal
        // owner state. Retained terminal receipts arrive as LiveCell.
        codex_code_mode::WaitOutcome::MissingCell(_) => return None,
    };
    let state = match response {
        response if super::observation_failed(response) => return None,
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
    steering_activity: impl Future<Output = ()>,
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
    let decided = |result: Result<codex_code_mode::WaitOutcome, String>| {
        result
            .map(|response| OwnerHeldCodeModeWait {
                exit: OwnerHeldCodeModeExit::Runtime(response),
                drained_observations: 0,
            })
            .map_err(|message| OwnerHeldCodeModeWaitError {
                message,
                drained_observations: 0,
            })
    };
    if cancellation_token.is_cancelled() {
        return Err(cancelled());
    }
    tokio::pin!(input_activity);
    tokio::pin!(steering_activity);
    // The runtime retains a decision only while no observer holds it; one
    // delivered to an observer that is then dropped is lost. Answer input that
    // is already waiting before starting an observation.
    if let Some(activity) = input_activity.as_mut().now_or_never() {
        if activity != InputQueueActivity::Mailbox {
            return Ok(steered(activity));
        }
        // Mail queued while the model was writing this call would preempt a
        // cell that is about to finish and cost a model round trip only to
        // observe its completion. The mail stays queued for the next sampling
        // request either way, so let an imminent decision arrive first.
        return tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => Err(cancelled()),
            result = wait_once() => decided(result),
            _ = &mut steering_activity => Ok(steered(InputQueueActivity::Steer)),
            _ = tokio::time::sleep(MAILBOX_PREEMPTION_GRACE) => Ok(steered(activity)),
        };
    }
    let observation = wait_once();
    tokio::pin!(observation);
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(cancelled()),
        // A decision delivered in the same wakeup as new input wins; the input
        // stays queued for the next sampling request.
        result = &mut observation => decided(result),
        activity = &mut input_activity => {
            if activity != InputQueueActivity::Mailbox {
                return Ok(steered(activity));
            }
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => Err(cancelled()),
                result = &mut observation => decided(result),
                _ = &mut steering_activity => Ok(steered(InputQueueActivity::Steer)),
                _ = tokio::time::sleep(MAILBOX_PREEMPTION_GRACE) => Ok(steered(activity)),
            }
        }
    }
}

pub(super) async fn queued_input_activity(
    exec: &ExecContext,
    turn_state: Option<&tokio::sync::Mutex<crate::state::TurnState>>,
    activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
) -> InputQueueActivity {
    queued_activity(exec, turn_state, activity_rx, false).await
}

pub(super) async fn queued_steering_activity(
    exec: &ExecContext,
    turn_state: Option<&tokio::sync::Mutex<crate::state::TurnState>>,
    activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
) {
    queued_activity(exec, turn_state, activity_rx, true).await;
}

async fn queued_activity(
    exec: &ExecContext,
    turn_state: Option<&tokio::sync::Mutex<crate::state::TurnState>>,
    mut activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
    steering_only: bool,
) -> InputQueueActivity {
    loop {
        // Mark the wake seen before reading retained state: anything arriving
        // during the read is either in that state or leaves another wake.
        let internal_completion =
            *activity_rx.borrow_and_update() == InputQueueActivity::InternalCompletion;
        if let Some(activity) = exec
            .session
            .input_queue
            .pending_activity(turn_state, internal_completion)
            .await
            && (!steering_only || activity == InputQueueActivity::Steer)
        {
            return activity;
        }
        if activity_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}


fn unchanged_wait_receipt(
    cell_id: &codex_code_mode::CellId,
    suppressed_continuation_count: u32,
) -> TurnTimingDeterministicContinuationReceipt {
    let resource_identity_hash = hex::encode(Sha256::digest(
        format!("code-mode-cell\0{}", cell_id.as_str()).as_bytes(),
    ));
    TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::UnchangedWait,
        wire_identity: String::new(),
        resource_identity_hash,
        // Cell ids are allocated from a never-reused lifecycle namespace by
        // the code-mode owner. Pair that lifecycle identity with its running
        // phase instead of reporting one constant revision for every cell.
        state_revision: hex::encode(Sha256::digest(format!(
            "code-mode-cell-lifecycle\0{}\0running",
            cell_id.as_str()
        ))),
        host_action: DeterministicContinuationHostAction::AwaitStateChange,
        action_bounds_hash: hex::encode(Sha256::digest(
            b"operation-lifetime:cell-terminal-or-turn-cancellation",
        )),
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
    fn tool_execution_timing(&self) -> crate::tools::registry::ToolExecutionTiming {
        // Like exec, this wrapper includes idle cell orchestration. Only the
        // nested tools own tool-active intervals.
        crate::tools::registry::ToolExecutionTiming::NestedRuntime
    }

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


