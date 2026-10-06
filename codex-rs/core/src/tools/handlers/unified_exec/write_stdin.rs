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
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_async_utils::OrCancelExt;
use serde::Deserialize;
use std::sync::Arc;

use super::super::shell_spec::create_write_stdin_tool_with_max_timeout;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: u32,
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
        Box::pin(self.handle_call(invocation))
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
        validate_independent_review_stdin(&turn.session_source, &args.chars)
            .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
        if args.terminate
            && !session.services.unified_exec_manager
                .terminate_process_for_poll(args.session_id).await
        {
            return Err(FunctionCallError::RespondToModel(
                "termination could not be confirmed; inspect the session before retrying".to_string(),
            ));
        }
        let yield_time_ms = owner_wait_yield_time_ms(
            &args.chars,
            args.yield_time_ms,
            matches!(source, ToolCallSource::CodeMode { .. }),
        );
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
            let response = if wait_for_output {
                manager.write_stdin_until_output(request)
                    .or_cancel(&cancellation_token).await
                    .map_err(|_| FunctionCallError::RespondToModel("command wait cancelled; process state remains inspectable".to_string()))?
            } else {
                manager.write_stdin(request).await
            };
            if !wait_for_output || response.as_ref().map_or(true, |output| {
                output.process_exited || !output.raw_output.is_empty()
                    || !output.pending_deferred_completions.is_empty()
                    || nested_return_margin_reached(source.nested_deadline(), std::time::Instant::now())
            }) {
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
            Err(crate::unified_exec::UnifiedExecError::ProcessFailedWithOutput { mut output, .. }) => {
                output.max_output_tokens = args.max_output_tokens;
                output.truncation_policy = turn.model_info.truncation_policy.into();
                Ok(*output)
            }
            other => other,
        };
        let mut response = response.map_err(|err| match err {
            crate::unified_exec::UnifiedExecError::ToolHistoryPersistence { message, .. } => {
                FunctionCallError::Fatal(message)
            }
            err => FunctionCallError::RespondToModel(format!("write_stdin failed: {err}")),
        })?;

        if let Some(running) = session
            .services
            .command_execution
            .running_process(args.session_id)
            .await
        {
            let artifact = response
                .raw_output_artifact
                .clone()
                .unwrap_or_else(|| running.artifact.clone());
            response.raw_output_artifact = Some(artifact.clone());
            if response.process_id.is_some() {
                session
                    .services
                    .command_execution
                    .update_running_artifact(args.session_id, artifact)
                    .await;
            } else {
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

        response.prepare_recovery_artifact(
            turn.config.codex_home.as_path(), &session.thread_id.to_string(),
        ).await;
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
    fn permits_shared_workspace_observation(&self, payload: &ToolPayload) -> bool {
        let ToolPayload::Function { arguments } = payload else {
            return false;
        };
        serde_json::from_str::<serde_json::Value>(arguments).is_ok_and(|arguments| {
            arguments.get("chars").is_none_or(|chars| chars.as_str() == Some(""))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_nested_wait_preserves_the_manager_return_margin() {
        let now = std::time::Instant::now();
        let margin = crate::unified_exec::NESTED_POLL_MARGIN;
        assert!(!nested_return_margin_reached(None, now));
        assert!(nested_return_margin_reached(Some(now - margin), now));
        assert!(nested_return_margin_reached(Some(now), now));
        assert!(nested_return_margin_reached(Some(now + margin), now));
        assert!(!nested_return_margin_reached(
            Some(now + margin + std::time::Duration::from_nanos(1)), now,
        ));
    }

    #[test]
    fn passive_poll_default_preserves_explicit_bounds_and_writes() {
        for (fields, passive) in [
            (serde_json::json!({}), true),
            (serde_json::json!({"chars":""}), true),
            (serde_json::json!({"wait_for_output":false}), false),
            (serde_json::json!({"yield_time_ms":1000}), false),
            (serde_json::json!({"chars":"input"}), false),
            (serde_json::json!({"terminate":true}), false),
            (serde_json::json!({"yield_time_ms":1000,"wait_for_output":true}), true),
        ] {
            let mut value = fields;
            value["session_id"] = 7.into();
            let args: WriteStdinArgs = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(args.waits_for_output(), passive, "{value}");
        }
    }

    #[test]
    fn accepts_full_unsigned_session_id_range() {
        let args: WriteStdinArgs =
            serde_json::from_str(r#"{"session_id":2374420115,"chars":"","yield_time_ms":1000}"#)
                .expect("session id returned by exec_command should deserialize");

        assert_eq!(args.session_id, 2_374_420_115);
        assert_eq!(args.yield_time_ms, Some(1_000));
    }

    #[test]
    fn empty_poll_uses_one_owner_wait_deadline() {
        for nested in [false, true] {
            assert_eq!(owner_wait_yield_time_ms("", Some(5_000), nested), 5_000);
            assert_eq!(owner_wait_yield_time_ms("", Some(250), nested), 250);
            assert_eq!(owner_wait_yield_time_ms("", Some(120_000), nested), 120_000);
            assert_eq!(owner_wait_yield_time_ms("input", None, nested), 250);
            assert_eq!(
                owner_wait_yield_time_ms("input", Some(1_000), nested),
                1_000
            );
        }
        assert_eq!(owner_wait_yield_time_ms("", None, false), 300_000);
    }

    #[test]
    fn nested_default_poll_returns_before_its_cell_yields() {
        // The code-mode cell hands control back after five silent minutes; an
        // equal poll default always lost that race and cost a `wait` call.
        let nested = owner_wait_yield_time_ms("", None, true);
        assert_eq!(nested, 285_000);
        assert!(nested < 300_000);
    }
}
