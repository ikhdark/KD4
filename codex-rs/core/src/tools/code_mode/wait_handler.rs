use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::future::Future;
use std::time::Duration;
use std::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::ExecContext;
use super::WAIT_TOOL_NAME;
use super::handle_runtime_response;
use super::wait_spec::create_wait_tool;

pub struct CodeModeWaitHandler;

const MAX_UNCHANGED_OBSERVATIONS: u32 = 256;
const MAX_HOST_HELD_WAIT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Deserialize)]
struct ExecWaitArgs {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    DEFAULT_WAIT_YIELD_TIME_MS
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
            turn,
            cancellation_token,
            tool_name,
            payload,
            ..
        } = invocation;

        match payload {
            ToolPayload::Function { arguments }
                if tool_name.namespace.is_none() && tool_name.name.as_str() == WAIT_TOOL_NAME =>
            {
                let args: ExecWaitArgs = parse_arguments(&arguments)?;
                let exec = ExecContext { session, turn };
                let started_at = Instant::now();
                let cell_id = codex_code_mode::CellId::new(args.cell_id);
                let (wait_response, unchanged_observations) = if args.terminate {
                    exec.session
                        .services
                        .code_mode_service
                        .terminate(cell_id.clone())
                        .await
                        .map(|response| (response, 0))
                } else {
                    let deadline = started_at + MAX_HOST_HELD_WAIT;
                    hold_unchanged_waits(
                        || {
                            exec.session.services.code_mode_service.wait(
                                codex_code_mode::WaitRequest {
                                    cell_id: cell_id.clone(),
                                    yield_time_ms: args.yield_time_ms,
                                },
                            )
                        },
                        &cancellation_token,
                        deadline,
                        &cell_id,
                    )
                    .await
                }
                .map_err(FunctionCallError::RespondToModel)?;
                if let codex_code_mode::WaitOutcome::LiveCell(response) = &wait_response
                    && !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. })
                {
                    // Only a live-cell wait can close a CodeCell. A missing
                    // cell is still an ordinary `wait` tool result, but there
                    // is no runtime object for the reducer to complete.
                    let runtime_cell_id = match response {
                        codex_code_mode::RuntimeResponse::Yielded { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Terminated { cell_id, .. }
                        | codex_code_mode::RuntimeResponse::Result { cell_id, .. } => cell_id,
                    };
                    exec.session
                        .services
                        .rollout_thread_trace
                        .code_cell_trace_context(
                            exec.turn.sub_id.as_str(),
                            runtime_cell_id.as_str(),
                        )
                        .record_ended(response);
                    exec.session
                        .services
                        .code_mode_service
                        .finish_cell_dispatch(runtime_cell_id);
                }
                exec.session.services.elicitations.wait_until_clear().await;
                let mut output = handle_runtime_response(
                    &exec,
                    wait_response.into(),
                    args.max_tokens,
                    started_at,
                )
                .await
                .map_err(FunctionCallError::RespondToModel)?;
                if unchanged_observations > 0 {
                    output = output.with_deterministic_continuation_receipt(
                        unchanged_wait_receipt(&cell_id, unchanged_observations),
                    );
                }
                Ok(boxed_tool_output(output))
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
        }
    }
}

async fn hold_unchanged_waits<F, Fut>(
    mut wait_once: F,
    cancellation_token: &tokio_util::sync::CancellationToken,
    deadline: Instant,
    cell_id: &codex_code_mode::CellId,
) -> Result<(codex_code_mode::WaitOutcome, u32), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<codex_code_mode::WaitOutcome, String>>,
{
    let mut unchanged_observations = 0_u32;
    loop {
        let response = tokio::select! {
            result = wait_once() => result?,
            _ = cancellation_token.cancelled() => {
                return Err("wait cancelled".to_string());
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                codex_code_mode::WaitOutcome::LiveCell(
                    codex_code_mode::RuntimeResponse::Yielded {
                        cell_id: cell_id.clone(),
                        content_items: Vec::new(),
                    },
                )
            }
        };
        if is_empty_live_yield(&response) {
            unchanged_observations = unchanged_observations.saturating_add(1);
            if unchanged_observations >= MAX_UNCHANGED_OBSERVATIONS || Instant::now() >= deadline {
                return Ok((response, unchanged_observations));
            }
            continue;
        }
        return Ok((response, unchanged_observations));
    }
}

fn is_empty_live_yield(outcome: &codex_code_mode::WaitOutcome) -> bool {
    matches!(
        outcome,
        codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded {
            content_items,
            ..
        }) if content_items.is_empty()
    )
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
        resource_identity_hash,
        state_revision: "live-empty-v1".to_string(),
        host_action: DeterministicContinuationHostAction::AwaitStateChange,
        suppressed_continuation_count,
        avoided_token_usage: None,
    }
}

impl CoreToolRuntime for CodeModeWaitHandler {
    fn pre_tool_use_payload(&self, _invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        // Code-mode `wait` is runtime control for an existing code cell, not a
        // standalone user action. Tool calls made from code mode still flow
        // through normal dispatch, but hooks should not block or rewrite the
        // wait loop itself.
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
    use std::collections::VecDeque;

    fn empty_yield(cell_id: &codex_code_mode::CellId) -> codex_code_mode::WaitOutcome {
        codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
        })
    }

    #[tokio::test]
    async fn ten_unchanged_observations_are_held_until_one_meaningful_transition() {
        let cell_id = codex_code_mode::CellId::new("cell-1".to_string());
        let meaningful =
            codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded {
                cell_id: cell_id.clone(),
                content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: "meaningful".to_string(),
                }],
            });
        let mut observations = VecDeque::from(
            std::iter::repeat_with(|| Ok(empty_yield(&cell_id)))
                .take(10)
                .chain(std::iter::once(Ok(meaningful)))
                .collect::<Vec<_>>(),
        );

        let (outcome, unchanged) = hold_unchanged_waits(
            || std::future::ready(observations.pop_front().expect("scripted observation")),
            &tokio_util::sync::CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            &cell_id,
        )
        .await
        .expect("held wait");

        assert!(matches!(
            outcome,
            codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::Yielded { content_items, .. }
            ) if !content_items.is_empty()
        ));
        assert_eq!(unchanged, 10);
        assert!(observations.is_empty());
        let receipt = unchanged_wait_receipt(&cell_id, unchanged);
        assert_eq!(receipt.suppressed_continuation_count, 10);
        assert_eq!(receipt.avoided_token_usage, None);
    }

    #[tokio::test]
    async fn held_wait_preserves_error_and_cancellation() {
        let cell_id = codex_code_mode::CellId::new("cell-2".to_string());
        let error = hold_unchanged_waits(
            || std::future::ready(Err("runtime failed".to_string())),
            &tokio_util::sync::CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            &cell_id,
        )
        .await;
        assert_eq!(error, Err("runtime failed".to_string()));

        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let cancelled = hold_unchanged_waits(
            std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
            &cancellation,
            Instant::now() + Duration::from_secs(1),
            &cell_id,
        )
        .await;
        assert_eq!(cancelled, Err("wait cancelled".to_string()));
    }

    #[tokio::test]
    async fn held_wait_stops_at_deadline_and_unchanged_observation_bound() {
        let cell_id = codex_code_mode::CellId::new("cell-3".to_string());
        let (deadline_outcome, deadline_count) = hold_unchanged_waits(
            std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
            &tokio_util::sync::CancellationToken::new(),
            Instant::now(),
            &cell_id,
        )
        .await
        .expect("deadline result");
        assert!(is_empty_live_yield(&deadline_outcome));
        assert_eq!(deadline_count, 1);

        let (bounded_outcome, bounded_count) = hold_unchanged_waits(
            || std::future::ready(Ok(empty_yield(&cell_id))),
            &tokio_util::sync::CancellationToken::new(),
            Instant::now() + Duration::from_secs(1),
            &cell_id,
        )
        .await
        .expect("bounded result");
        assert!(is_empty_live_yield(&bounded_outcome));
        assert_eq!(bounded_count, MAX_UNCHANGED_OBSERVATIONS);
    }
}
