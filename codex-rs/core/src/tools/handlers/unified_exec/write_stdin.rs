use crate::FunctionCallError;
use crate::agent::task_capabilities::validate_independent_review_stdin;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS;
use crate::unified_exec::WriteStdinRequest;
use codex_async_utils::OrCancelExt;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::sync::Arc;

use super::super::shell_spec::create_write_stdin_tool_with_max_timeout;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: u32,
    #[serde(default)]
    incarnation: Option<uuid::Uuid>,
    #[serde(default)]
    chars: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    wait_for_output: Option<bool>,
    #[serde(default)]
    terminate: bool,
}

impl WriteStdinArgs {
    fn waits_for_output(&self) -> bool {
        self.wait_for_output.unwrap_or_else(|| {
            self.chars.is_empty() && self.yield_time_ms.is_none() && !self.terminate
        })
    }
}

pub struct WriteStdinHandler {
    max_timeout_ms: u64,
}

impl WriteStdinHandler {
    pub(crate) fn new(max_timeout_ms: u64) -> Self {
        Self { max_timeout_ms }
    }
}

impl Default for WriteStdinHandler {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

impl ToolExecutor<ToolInvocation> for WriteStdinHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write_stdin")
    }

    fn spec(&self) -> ToolSpec {
        create_write_stdin_tool_with_max_timeout(self.max_timeout_ms)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result = self.handle_call(invocation).await;
            tracing::info!(event.name = "codex.unified_exec.handler_elapsed", tool = "write_stdin",
                end_to_end_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                transport_success = result.is_ok(), endpoint = "handler_return",
                "handler elapsed includes queueing and finalization; distinct from observation wall_time");
            result
        })
    }
}

fn completed_process_output(
    mut result: serde_json::Value,
) -> Box<dyn crate::tools::context::ToolOutput> {
    let retained_signal = result.as_object_mut().and_then(|result| result.remove("_sampling_request_signal"));
    let success = result["success"].as_bool().unwrap_or(false);
    let output = codex_tools::JsonToolOutput::with_success(result, Some(success));
    if let Some(signal) = retained_signal {
        return boxed_tool_output(output.with_sampling_request_signal(signal));
    }
    if success {
        boxed_tool_output(output)
    } else {
        boxed_tool_output(output.with_sampling_request_signal(serde_json::json!({
            "outcome": "failure",
            "failure": {"fingerprint": "unified_exec.completed_process_recovery"}
        })))
    }
}


impl WriteStdinHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            payload,
            source,
            cancellation_token,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "write_stdin handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WriteStdinArgs = parse_arguments(&arguments)?;
        let live_incarnation = session
            .services
            .unified_exec_manager
            .process_incarnation(args.session_id)
            .await;
        if live_incarnation.is_none()
            && args.chars.is_empty()
            && !args.terminate
            && let Some(incarnation) = args.incarnation
            && let Some(result) = session
                .services
                .command_execution
                .completed_process_result(args.session_id, incarnation)
                .await
        {
            return Ok(completed_process_output(result));
        }
        // Legacy numeric-only handles are readable history, not live authority.
        // IDs are not reused inside a manager; the creation token additionally
        // prevents persisted values from controlling another runtime's process.
        let incarnation = match args.incarnation {
            Some(incarnation) if live_incarnation == Some(incarnation) => incarnation,
            _ => return Err(FunctionCallError::RespondToModel(
                "process handle is stale or lacks its creation identity; supply incarnation from the originating session_capabilities. No process was polled, written, or terminated.".into(),
            )),
        };
        let wait_for_output = args.waits_for_output();
        if args.terminate && !args.chars.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "terminate requires empty chars; send input separately".to_string(),
            ));
        }
        if wait_for_output && !args.chars.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "wait_for_output requires empty chars; send input separately".to_string(),
            ));
        }
        validate_independent_review_stdin(&turn.session_source, &args.chars, args.terminate)
            .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
        if args.terminate
            && !session
                .services
                .unified_exec_manager
                .terminate_process_for_poll(args.session_id)
                .await
        {
            return Err(FunctionCallError::RespondToModel(
                "termination could not be confirmed; inspect the session before retrying"
                    .to_string(),
            ));
        }
        let yield_time_ms = owner_wait_yield_time_ms(
            &args.chars,
            args.yield_time_ms,
            matches!(source, ToolCallSource::CodeMode { .. }),
        );
        let passive_wait_started = std::time::Instant::now();
        let mut observed_wall_time = std::time::Duration::ZERO;
        let response = loop {
            let request = WriteStdinRequest {
                process_id: args.session_id,
                input: &args.chars,
                yield_time_ms,
                max_output_tokens: args.max_output_tokens,
                truncation_policy: turn.model_info.truncation_policy.into(),
                // A nested poll must finish inside the runtime's wrapper
                // deadline; the wrapper cancels rather than waits.
                nested_deadline: source.nested_deadline(),
            };
            let manager = &session.services.unified_exec_manager;
            let mut response = if wait_for_output {
                manager
                    .write_stdin_until_output(request)
                    .or_cancel(&cancellation_token)
                    .await
                    .map_err(|_| {
                        FunctionCallError::RespondToModel(
                            "command wait cancelled; process state remains inspectable".to_string(),
                        )
                    })?
            } else {
                manager.write_stdin(request).await
            };
            if let Ok(output) = &mut response {
                observed_wall_time = observed_wall_time.saturating_add(output.wall_time);
                output.wall_time = observed_wall_time;
            }
            if !wait_for_output
                || response.as_ref().map_or(true, |output| {
                    output.process_exited
                        || !output.raw_output.is_empty()
                        || !output.pending_deferred_completions.is_empty()
                        || nested_return_margin_reached(
                            source.nested_deadline(),
                            std::time::Instant::now(),
                        )
                })
            {
                break response;
            }
        };
        if let Err(crate::unified_exec::UnifiedExecError::ToolHistoryPersistence {
            event_call_id: Some(call_id),
            ..
        }) = &response
            && !args.chars.is_empty()
        {
            session
                .send_event(
                    turn.as_ref(),
                    EventMsg::TerminalInteraction(TerminalInteractionEvent {
                        call_id: call_id.clone(),
                        process_id: args.session_id.to_string(),
                        stdin: args.chars.clone(),
                    }),
                )
                .await;
        }
        let response = match response {
            Err(crate::unified_exec::UnifiedExecError::UnknownProcessId { process_id })
                if args.chars.is_empty() && !args.terminate =>
            {
                if let Some(result) = session
                    .services
                    .command_execution
                    .completed_process_result(process_id, incarnation)
                    .await
                {
                    return Ok(completed_process_output(result));
                }
                Err(crate::unified_exec::UnifiedExecError::UnknownProcessId { process_id })
            }
            Err(crate::unified_exec::UnifiedExecError::ProcessFailedWithOutput {
                mut output,
                ..
            }) => {
                output.max_output_tokens = args.max_output_tokens;
                output.truncation_policy = turn.model_info.truncation_policy.into();
                Ok(*output)
            }
            other => other,
        };
        let mut response = match response {
            Ok(response) => response,
            Err(crate::unified_exec::UnifiedExecError::ToolHistoryPersistence {
                message,
                output,
                ..
            }) => {
                return Err(FunctionCallError::Fatal(
                    super::preserve_durability_failure_output(
                        message,
                        output,
                        &turn.config.codex_home,
                        &session.thread_id.to_string(),
                    )
                    .await,
                ));
            }
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "write_stdin failed: {err}"
                )));
            }
        };

        if wait_for_output {
            // A passive wait may span several manager observation windows.
            // Report the entire owner wait, never just its final window.
            response.wall_time = passive_wait_started.elapsed();
        }

        if response.process_id.is_none()
            && let Some(cwd) = session.services.command_execution
                .process_local_cwd(args.session_id, incarnation).await
        {
            super::exec_command::attach_missing_rg_path_advisory(&mut response, &cwd, &turn)
                .await;
        }

        if let Some(running) = session
            .services
            .command_execution
            .running_process(args.session_id)
            .await
        {
            if let Some(cwd) = running.key.local_cwd() {
                super::exec_command::attach_hidden_rg_directory_advisory(
                    &mut response,
                    &cwd,
                    &turn,
                    &session.services.git_workspace,
                )
                .await;
            }
            let artifact = response
                .raw_output_artifact
                .clone()
                .unwrap_or_else(|| running.artifact.clone());
            response.raw_output_artifact = Some(artifact.clone());
            session
                .services
                .command_execution
                .update_running_artifact(args.session_id, artifact)
                .await;
            if response.process_id.is_none() {
                session
                    .services
                    .command_execution
                    .finish_running_process_with_execution_id(
                        args.session_id,
                        running.execution_id,
                        &running.parent_tool_execution_id,
                        response.exit_code,
                    )
                    .await;
            }
        }

        // Empty stdin is a background poll, so emit it only while there is
        // still a live process for the UI to wait on. Non-empty stdin is a real
        // terminal interaction and should remain visible even if it completes
        // the process before the response returns.
        if !args.chars.is_empty() || response.process_id.is_some() {
            let process_id = response.process_id.unwrap_or(args.session_id);
            let interaction = TerminalInteractionEvent {
                call_id: response.event_call_id.clone(),
                process_id: process_id.to_string(),
                stdin: args.chars.clone(),
            };
            session
                .send_event(turn.as_ref(), EventMsg::TerminalInteraction(interaction))
                .await;
        }

        response
            .prepare_recovery_artifact(
                turn.config.codex_home.as_path(),
                &session.thread_id.to_string(),
            )
            .await;
        Ok(boxed_tool_output(response))
    }
}

fn nested_return_margin_reached(
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    // The manager already ended observation early enough to return a handle.
    // Do not consume its transport headroom by entering another empty wait.
    deadline.is_some_and(|deadline| {
        deadline.saturating_duration_since(now) <= crate::unified_exec::NESTED_POLL_MARGIN
    })
}

fn owner_wait_yield_time_ms(
    chars: &str,
    requested_yield_time_ms: Option<u64>,
    nested: bool,
) -> u64 {
    if chars.is_empty() {
        // Omitted deadlines favor unattended waits. The process manager permits
        // a short empty poll only when output is pending; otherwise it floors
        // the requested wait at five seconds.
        // A nested poll must still return before its code-mode cell yields.
        requested_yield_time_ms.unwrap_or(if nested {
            u64::try_from(crate::tools::code_mode::NESTED_DEFAULT_POLL.as_millis())
                .unwrap_or(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
        } else {
            DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS
        })
    } else {
        requested_yield_time_ms.unwrap_or_else(super::default_write_stdin_yield_time_ms)
    }
}

impl CoreToolRuntime for WriteStdinHandler {
    fn controls_process(&self, payload: &ToolPayload) -> bool {
        let ToolPayload::Function { arguments } = payload else {
            return false;
        };
        serde_json::from_str::<WriteStdinArgs>(arguments)
            .is_ok_and(|args| args.terminate && args.chars.is_empty())
    }

    fn permits_shared_workspace_observation(&self, payload: &ToolPayload) -> bool {
        let ToolPayload::Function { arguments } = payload else {
            return false;
        };
        serde_json::from_str::<WriteStdinArgs>(arguments)
            .is_ok_and(|args| args.chars.is_empty() && !args.terminate)
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_hook_name(
        &self,
        _tool_name: &codex_tools::ToolName,
        _payload: &ToolPayload,
    ) -> Option<HookToolName> {
        None
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // `write_stdin` is transport for an existing exec session. Empty writes
        // are background polls, and non-empty writes continue a command that
        // already ran PreToolUse as Bash, so do not emit a second pre hook here.
        None
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Function { .. }).then(HookToolName::exec_command)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        // A `write_stdin` poll can observe final completion for the original
        // `exec_command`; emit that command's matching Bash PostToolUse.
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

