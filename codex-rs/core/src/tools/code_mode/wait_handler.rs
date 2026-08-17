use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;
use std::future::Future;
use std::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::session::InputQueueActivity;
use crate::tools::context::FunctionToolOutput;
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

pub(super) fn effective_observation_yield_time_ms(yield_time_ms: u64) -> u64 {
    // A zero-duration owner-held observation would turn a formerly visible
    // yield loop into a host-side busy loop. Reuse the existing default
    // observation cadence without treating it as a completion deadline.
    if yield_time_ms == 0 {
        DEFAULT_WAIT_YIELD_TIME_MS
    } else {
        yield_time_ms
    }
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
                    let (activity_rx, pending_activity) = exec
                        .session
                        .input_queue
                        .subscribe_activity(turn_state.as_deref())
                        .await;
                    let yield_time_ms = effective_observation_yield_time_ms(args.yield_time_ms);
                    let held = hold_unchanged_waits(
                        || {
                            exec.session.services.code_mode_service.wait(
                                codex_code_mode::WaitRequest {
                                    cell_id: cell_id.clone(),
                                    yield_time_ms,
                                },
                            )
                        },
                        &cancellation_token,
                        activity_rx,
                        pending_activity,
                        "wait cancelled",
                    )
                    .await;
                    let held = match held {
                        Ok(held) => held,
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
                output =
                    attach_drained_wait_evidence(&exec, output, &cell_id, drained_observations);
                if let Some(signal) = authoritative_wait_signal {
                    output = output.with_sampling_request_signal(signal);
                }
                Ok(boxed_tool_output(output))
            }
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{WAIT_TOOL_NAME} expects JSON arguments"
            ))),
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
        codex_code_mode::RuntimeResponse::Yielded { .. } => return None,
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

pub(super) async fn hold_unchanged_waits<F, Fut>(
    mut wait_once: F,
    cancellation_token: &tokio_util::sync::CancellationToken,
    mut activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
    mut pending_activity: Option<InputQueueActivity>,
    cancellation_message: &'static str,
) -> Result<OwnerHeldCodeModeWait, OwnerHeldCodeModeWaitError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<codex_code_mode::WaitOutcome, String>>,
{
    let mut drained_observations = 0_u32;
    loop {
        let response = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                return Err(OwnerHeldCodeModeWaitError {
                    message: cancellation_message.to_string(),
                    drained_observations,
                });
            }
            activity = next_input_activity(&mut activity_rx, &mut pending_activity) => {
                return Ok(OwnerHeldCodeModeWait {
                    exit: OwnerHeldCodeModeExit::InputActivity(activity),
                    drained_observations,
                });
            }
            result = wait_once() => {
                match result {
                    Ok(response) => response,
                    Err(message) => {
                        return Err(OwnerHeldCodeModeWaitError {
                            message,
                            drained_observations,
                        });
                    }
                }
            }
        };
        if is_empty_live_yield(&response) {
            drained_observations = drained_observations.saturating_add(1);
            tokio::task::yield_now().await;
            continue;
        }
        return Ok(OwnerHeldCodeModeWait {
            exit: OwnerHeldCodeModeExit::Runtime(response),
            drained_observations,
        });
    }
}

async fn next_input_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: &mut Option<InputQueueActivity>,
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

    fn empty_yield(cell_id: &codex_code_mode::CellId) -> codex_code_mode::WaitOutcome {
        codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
        })
    }

    #[test]
    fn terminal_signal_is_private_to_final_typed_cell_states() {
        let cell_id = codex_code_mode::CellId::new("cell-terminal".to_string());
        let terminal =
            codex_code_mode::WaitOutcome::LiveCell(codex_code_mode::RuntimeResponse::Result {
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
        assert!(terminal_wait_owner_signal(&empty_yield(&cell_id), &cell_id).is_none());
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

        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let held = hold_unchanged_waits(
            || std::future::ready(observations.pop_front().expect("scripted observation")),
            &tokio_util::sync::CancellationToken::new(),
            activity_rx,
            None,
            "wait cancelled",
        )
        .await
        .expect("held wait");

        assert!(matches!(
            held.exit,
            OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::Yielded { content_items, .. }
            )) if !content_items.is_empty()
        ));
        assert_eq!(held.drained_observations, 10);
        assert!(observations.is_empty());
        let receipt = unchanged_wait_receipt(&cell_id, held.drained_observations);
        assert_eq!(receipt.suppressed_continuation_count, 10);
    }

    #[tokio::test]
    async fn held_wait_preserves_error_and_cancellation() {
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let error = hold_unchanged_waits(
            || std::future::ready(Err("runtime failed".to_string())),
            &tokio_util::sync::CancellationToken::new(),
            activity_rx,
            None,
            "wait cancelled",
        )
        .await;
        assert!(matches!(
            error,
            Err(OwnerHeldCodeModeWaitError { message, drained_observations: 0 })
                if message == "runtime failed"
        ));

        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let cancelled = hold_unchanged_waits(
            std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
            &cancellation,
            activity_rx,
            None,
            "wait cancelled",
        )
        .await;
        assert!(matches!(
            cancelled,
            Err(OwnerHeldCodeModeWaitError { message, drained_observations: 0 })
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
            hold_unchanged_waits(
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
                activity_rx,
                None,
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
    async fn arbitrary_empty_observation_count_does_not_end_held_wait() {
        let cell_id = codex_code_mode::CellId::new("cell-3".to_string());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancel = cancellation.clone();
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let mut observations = 0_u32;
        let held = hold_unchanged_waits(
            || {
                observations = observations.saturating_add(1);
                let should_block = observations > 300;
                let empty = empty_yield(&cell_id);
                async move {
                    if should_block {
                        std::future::pending().await
                    } else {
                        Ok(empty)
                    }
                }
            },
            &cancellation,
            activity_rx,
            None,
            "wait cancelled",
        );
        let cancelled = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel();
        });
        let error = held
            .await
            .expect_err("cancellation should end the held wait");
        cancelled.await.expect("cancellation task");
        assert_eq!(error.message, "wait cancelled");
        assert!(error.drained_observations > 256);
    }

    #[tokio::test(start_paused = true)]
    async fn elapsed_five_minutes_does_not_end_held_wait() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let wait_cancellation = cancellation.clone();
        let (_activity_tx, activity_rx) = tokio::sync::watch::channel(InputQueueActivity::Mailbox);
        let held = tokio::spawn(async move {
            hold_unchanged_waits(
                std::future::pending::<Result<codex_code_mode::WaitOutcome, String>>,
                &wait_cancellation,
                activity_rx,
                None,
                "wait cancelled",
            )
            .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5 * 60 + 1)).await;
        tokio::task::yield_now().await;
        assert!(!held.is_finished());

        cancellation.cancel();
        let error = held
            .await
            .expect("held wait task")
            .expect_err("owner cancellation should release held wait");
        assert_eq!(error.message, "wait cancelled");
    }

    #[test]
    fn unchanged_wait_revision_is_cell_lifecycle_specific() {
        let first = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-a".to_string()), 1);
        let same = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-a".to_string()), 2);
        let other = unchanged_wait_receipt(&codex_code_mode::CellId::new("cell-b".to_string()), 1);

        assert_eq!(first.state_revision, same.state_revision);
        assert_ne!(first.state_revision, other.state_revision);
    }

    #[test]
    fn zero_yield_uses_existing_default_internal_cadence() {
        assert_eq!(
            effective_observation_yield_time_ms(0),
            DEFAULT_WAIT_YIELD_TIME_MS
        );
        assert_eq!(effective_observation_yield_time_ms(25), 25);
    }
}
