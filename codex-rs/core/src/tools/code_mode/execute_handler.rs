use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::handle_runtime_response;
use super::is_exec_tool_name;
use super::wait_handler::OwnerHeldCodeModeExit;
use super::wait_handler::attach_drained_wait_evidence;
use super::wait_handler::hold_until_state_change;
use super::wait_handler::input_activity_response;
use super::wait_handler::record_internally_drained_waits;
use super::wait_handler::terminate_cancelled_cell;

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    direct_nested_tool_specs: Vec<ToolSpec>,
    deferred_nested_tool_specs: Vec<ToolSpec>,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(
        spec: ToolSpec,
        direct_nested_tool_specs: Vec<ToolSpec>,
        deferred_nested_tool_specs: Vec<ToolSpec>,
    ) -> Self {
        Self {
            spec,
            direct_nested_tool_specs,
            deferred_nested_tool_specs,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        code: String,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext { session, turn };
        let mut nested_tool_specs = self.direct_nested_tool_specs.clone();
        // Deferred tools stay out of the model-visible `exec` description, but
        // code running inside the isolate can discover and invoke them through
        // `ALL_TOOLS`. Build the runtime registry from the complete set so a
        // same-cell tool search can immediately call the tool it discovers.
        nested_tool_specs.extend(self.deferred_nested_tool_specs.clone());
        let enabled_tools = codex_tools::collect_code_mode_tool_definitions(&nested_tool_specs);
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(codex_code_mode::ExecuteRequest {
                tool_call_id: call_id.clone(),
                enabled_tools,
                source: args.code.clone(),
                // Initial observation is an internal hand-off from the
                // runtime to the code-mode owner. The owner applies the
                // requested cadence to all subsequent observations.
                yield_time_ms: Some(0),
                max_output_tokens: args.max_output_tokens,
            })
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        let runtime_cell_id = cell_id.to_string();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .start_code_cell_trace(
                exec.turn.sub_id.as_str(),
                runtime_cell_id.as_str(),
                call_id.as_str(),
                args.code.as_str(),
            );
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        let turn_state = exec
            .session
            .input_queue
            .turn_state_for_sub_id(&exec.session.active_turn, &exec.turn.sub_id)
            .await;
        let (activity_rx, pending_activity) = exec
            .session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        // Consume the immediate initial observation before making the held
        // wait steerable. This clears the runtime's initial observer, so
        // steering cannot leave a stale observer that rejects a later wait.
        let initial_response = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                terminate_cancelled_cell(&exec, &cell_id).await;
                return Err(FunctionCallError::RespondToModel("exec cancelled".to_string()));
            }
            response = started_cell.initial_response() => {
                response.map_err(FunctionCallError::RespondToModel)?
            }
        };
        let initial_is_empty = matches!(
            &initial_response,
            codex_code_mode::RuntimeResponse::Yielded { content_items, .. }
                if content_items.is_empty()
        );
        let (response, live_cell, drained_observations) = if initial_is_empty {
            let held = hold_until_state_change(
                || {
                    exec.session
                        .services
                        .code_mode_service
                        .wait_for_state_change(cell_id.clone())
                },
                &cancellation_token,
                activity_rx,
                pending_activity,
                "exec cancelled",
            )
            .await;
            let held = match held {
                Ok(held) => held,
                Err(mut error) => {
                    error.drained_observations = error.drained_observations.saturating_add(1);
                    record_internally_drained_waits(&exec, error.drained_observations);
                    if cancellation_token.is_cancelled() {
                        terminate_cancelled_cell(&exec, &cell_id).await;
                    }
                    return Err(FunctionCallError::RespondToModel(error.message));
                }
            };
            let drained_observations = held.drained_observations.saturating_add(1);
            match held.exit {
                OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                    response,
                )) => (response, true, drained_observations),
                OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::MissingCell(
                    response,
                )) => (response, false, drained_observations),
                OwnerHeldCodeModeExit::InputActivity(activity) => (
                    input_activity_response(&cell_id, activity),
                    true,
                    drained_observations,
                ),
            }
        } else {
            (initial_response, true, 0)
        };
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        if live_cell && !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
        }
        exec.session.services.elicitations.wait_until_clear().await;
        let output = handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .map_err(FunctionCallError::RespondToModel)?;
        Ok(attach_drained_wait_evidence(
            &exec,
            output,
            &cell_id,
            drained_observations,
        ))
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            cancellation_token,
            ..
        } = invocation;

        match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(session, turn, call_id, input, cancellation_token)
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        // Cancellation must keep polling the handler through bounded cell
        // termination so the V8/runtime owner cannot be orphaned.
        true
    }

    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        // Nested tools own their actual execution timing. Treating the entire
        // JavaScript cell as a handler interval double-counts orchestration and
        // waits as tool runtime.
        ToolExecutionTiming::NestedRuntime
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}
