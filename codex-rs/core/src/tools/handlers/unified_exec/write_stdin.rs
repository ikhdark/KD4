use crate::agent::task_capabilities::validate_independent_review_stdin;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
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
use serde::Deserialize;
use std::time::Duration;
use std::time::Instant;

use super::super::shell_spec::create_write_stdin_tool;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: i32,
    #[serde(default)]
    chars: String,
    #[serde(default = "super::default_write_stdin_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub struct WriteStdinHandler;

impl ToolExecutor<ToolInvocation> for WriteStdinHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("write_stdin")
    }

    fn spec(&self) -> ToolSpec {
        create_write_stdin_tool()
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
            turn,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "write_stdin handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: WriteStdinArgs = parse_arguments(&arguments)?;
        validate_independent_review_stdin(&turn.session_source, &args.chars)
            .map_err(|message| FunctionCallError::RespondToModel(message.to_string()))?;
        let poll_started_at = Instant::now();
        let mut response = session
            .services
            .unified_exec_manager
            .write_stdin(WriteStdinRequest {
                process_id: args.session_id,
                input: &args.chars,
                yield_time_ms: args.yield_time_ms,
                max_output_tokens: args.max_output_tokens,
                truncation_policy: turn.model_info.truncation_policy.into(),
            })
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("write_stdin failed: {err}"))
            })?;

        let mut internally_drained_polls = 0u32;
        while should_hold_empty_poll(&args.chars, &response, poll_started_at.elapsed()) {
            let next = session
                .services
                .unified_exec_manager
                .write_stdin(WriteStdinRequest {
                    process_id: args.session_id,
                    input: "",
                    yield_time_ms: args.yield_time_ms,
                    max_output_tokens: args.max_output_tokens,
                    truncation_policy: turn.model_info.truncation_policy.into(),
                })
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("write_stdin failed: {err}"))
                })?;
            merge_poll_response(&mut response, next);
            internally_drained_polls = internally_drained_polls.saturating_add(1);
        }
        if internally_drained_polls > 0 {
            turn.turn_timing_state
                .record_internally_drained_waits(internally_drained_polls);
        }

        if let Some(running) = session
            .services
            .command_execution
            .running_process(args.session_id)
            .await
        {
            let artifact = response
                .raw_output_artifact
                .clone()
                .unwrap_or(running.artifact);
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
                    .finish_running_process(args.session_id, response.exit_code)
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

        Ok(boxed_tool_output(response))
    }
}

fn should_hold_empty_poll(
    chars: &str,
    response: &ExecCommandToolOutput,
    elapsed: Duration,
) -> bool {
    chars.is_empty()
        && response.process_id.is_some()
        && response.raw_output.iter().all(u8::is_ascii_whitespace)
        && elapsed < Duration::from_millis(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
}

fn merge_poll_response(current: &mut ExecCommandToolOutput, next: ExecCommandToolOutput) {
    let mut raw_output = std::mem::take(&mut current.raw_output);
    raw_output.extend_from_slice(&next.raw_output);
    let wall_time = current.wall_time.saturating_add(next.wall_time);
    let original_token_count = match (current.original_token_count, next.original_token_count) {
        (Some(current), Some(next)) => Some(current.saturating_add(next)),
        (current, next) => current.or(next),
    };
    *current = next;
    current.raw_output = raw_output;
    current.wall_time = wall_time;
    current.original_token_count = original_token_count;
}

impl CoreToolRuntime for WriteStdinHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // `write_stdin` is transport for an existing exec session. Empty writes
        // are background polls, and non-empty writes continue a command that
        // already ran PreToolUse as Bash, so do not emit a second pre hook here.
        None
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
    use codex_utils_output_truncation::TruncationPolicy;

    fn response(raw_output: &[u8], process_id: Option<i32>) -> ExecCommandToolOutput {
        ExecCommandToolOutput {
            event_call_id: "call".to_string(),
            chunk_id: "chunk".to_string(),
            wall_time: Duration::from_millis(5),
            raw_output: raw_output.to_vec(),
            truncation_policy: TruncationPolicy::Bytes(1024),
            max_output_tokens: None,
            process_id,
            exit_code: None,
            original_token_count: Some(1),
            hook_command: None,
            raw_output_artifact: None,
            repair_notice: None,
        }
    }

    #[test]
    fn empty_unchanged_poll_is_owner_drained_until_actionable() {
        let pending = response(b"\r\n", Some(7));
        assert!(should_hold_empty_poll("", &pending, Duration::from_secs(1)));
        assert!(!should_hold_empty_poll(
            "input",
            &pending,
            Duration::from_secs(1)
        ));
        assert!(!should_hold_empty_poll(
            "",
            &response(b"ready", Some(7)),
            Duration::from_secs(1)
        ));
        assert!(!should_hold_empty_poll(
            "",
            &response(b"", None),
            Duration::from_secs(1)
        ));
        assert!(!should_hold_empty_poll(
            "",
            &pending,
            Duration::from_millis(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
        ));
    }

    #[test]
    fn owner_drained_poll_merge_preserves_all_observations() {
        let mut current = response(b"first\n", Some(7));
        let mut next = response(b"second\n", None);
        next.chunk_id = "final".to_string();
        next.exit_code = Some(0);

        merge_poll_response(&mut current, next);

        assert_eq!(current.raw_output, b"first\nsecond\n");
        assert_eq!(current.wall_time, Duration::from_millis(10));
        assert_eq!(current.original_token_count, Some(2));
        assert_eq!(current.chunk_id, "final");
        assert_eq!(current.process_id, None);
        assert_eq!(current.exit_code, Some(0));
    }
}
