use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecContext;
use super::UnifiedExecError;
pub(super) use super::head_tail_buffer::omitted_output_marker;
use super::process::ProcessOutputChunk;
use super::process::ProcessOutputSnapshot;
use super::process::UnifiedExecProcess;
use crate::exec::EXEC_OUTPUT_DELTA_CAP_NOTICE;
use crate::exec::OutputDeltaDecision;
use crate::exec::OutputDeltaLimiter;
use crate::session::session::Session;
use crate::session::turn::reconcile_turn_progress_event;
use crate::session::turn_context::TurnContext;
use crate::tools::command_execution::CommandExecutionId;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::command_execution::CompletionApplyResult;
use crate::tools::command_output_artifact::append_raw_output_artifact;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::known_delta_store::PreparedKnownDelta;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_features::Feature;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use codex_utils_path_uri::PathUri;

pub(crate) const TRAILING_OUTPUT_GRACE: Duration = Duration::from_millis(100);

/// Upper bound for a single ExecCommandOutputDelta chunk emitted by unified exec.
///
/// The unified exec output buffer already caps *retained* output (see
/// `UNIFIED_EXEC_OUTPUT_MAX_BYTES`), but we also cap per-event payload size so
/// downstream event consumers (especially app-server JSON-RPC) don't have to
/// process arbitrarily large delta payloads.
const UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES: usize = 8192;

struct OutputDrainedGuard {
    token: CancellationToken,
}

impl Drop for OutputDrainedGuard {
    fn drop(&mut self) {
        // Latch completion even when a waiter has not started yet. Multiple consumers wait for
        // output drain on fast commands, so this must wake all current and future waiters.
        self.token.cancel();
    }
}

/// Spawn a background task that continuously reads from the PTY, appends to the
/// shared transcript, and emits ExecCommandOutputDelta events on UTF‑8
/// boundaries.
// Preserve the shared unified-exec error shape at this process boundary.
#[allow(clippy::result_large_err)]
pub(crate) fn start_streaming_output(
    process: &Arc<UnifiedExecProcess>,
    context: &UnifiedExecContext,
    transcript: Arc<Mutex<HeadTailBuffer>>,
) -> Result<(), UnifiedExecError> {
    let Some(mut receiver) = process.take_output_receiver() else {
        return Err(UnifiedExecError::process_failed(
            "unified exec streaming output receiver was already taken".to_string(),
        ));
    };
    let output_handles = process.output_handles();
    let output_closed = output_handles.output_closed;
    let output_closed_notify = output_handles.output_closed_notify;
    let exit_token = output_handles.cancellation_token;
    let output_drained = process.output_drained_token();
    let process_ref = Arc::clone(process);

    let session_ref = Arc::clone(&context.session);
    let turn_ref = Arc::clone(&context.turn);
    let call_id = context.call_id.clone();

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        let _output_drained_guard = OutputDrainedGuard {
            token: output_drained,
        };

        let mut pending = PendingOutput::default();
        let emitted_deltas = OutputDeltaLimiter::default();

        let mut grace_sleep: Option<Pin<Box<Sleep>>> = None;
        let output_closed_notification = output_closed_notify.notified();
        tokio::pin!(output_closed_notification);
        output_closed_notification.as_mut().enable();
        let output_already_closed = output_closed.load(Ordering::Acquire);

        if output_already_closed {
            drain_queued_output(
                &mut receiver,
                &mut pending,
                &transcript,
                &call_id,
                &session_ref,
                &turn_ref,
                &emitted_deltas,
            )
            .await;
        } else {
            loop {
                tokio::select! {
                        biased;

                        _ = &mut output_closed_notification => {
                            drain_queued_output(
                                &mut receiver,
                                &mut pending,
                                &transcript,
                                &call_id,
                                &session_ref,
                                &turn_ref,
                                &emitted_deltas,
                            ).await;
                            break;
                        }

                    _ = exit_token.cancelled(), if grace_sleep.is_none() => {
                        let deadline = Instant::now() + TRAILING_OUTPUT_GRACE;
                        grace_sleep.replace(Box::pin(tokio::time::sleep_until(deadline)));
                    }

                    _ = async {
                        if let Some(sleep) = grace_sleep.as_mut() {
                            sleep.as_mut().await;
                        }
                    }, if grace_sleep.is_some() => {
                        process_ref.finish_termination();
                        drain_queued_output(
                            &mut receiver,
                            &mut pending,
                            &transcript,
                            &call_id,
                            &session_ref,
                            &turn_ref,
                            &emitted_deltas,
                        ).await;
                        break;
                    }

                    received = receiver.recv() => {
                        let chunk = match received {
                            Ok(chunk) => chunk,
                            Err(RecvError::Lagged(skipped)) => {
                                handle_lagged_output(
                                    skipped,
                                    &mut pending,
                                    &transcript,
                                    &call_id,
                                    &session_ref,
                                    &turn_ref,
                                    &emitted_deltas,
                                ).await;
                                continue;
                            },
                            Err(RecvError::Closed) => {
                                break;
                            }
                        };

                        process_chunk(
                            &mut pending,
                            &transcript,
                            &call_id,
                            &session_ref,
                            &turn_ref,
                            &emitted_deltas,
                            chunk,
                        ).await;
                    }
                }
            }
        }
        flush_pending(
            &mut pending,
            &transcript,
            &call_id,
            &session_ref,
            &turn_ref,
            &emitted_deltas,
        )
        .await;
    });
    Ok(())
}

pub(super) fn lagged_output_marker(skipped: u64) -> Vec<u8> {
    format!("\n[output unavailable: streaming receiver lagged by {skipped} chunk(s)]\n")
        .into_bytes()
}

struct ProcessOutputWaiterGuard(Arc<crate::turn_timing::TurnTimingState>);

impl ProcessOutputWaiterGuard {
    fn new(turn_timing: &Arc<crate::turn_timing::TurnTimingState>) -> Self {
        turn_timing.adjust_process_output_waiters(1);
        Self(Arc::clone(turn_timing))
    }
}

async fn wait_for_sticky_lifecycle_signal(signal: &CancellationToken) {
    let cancelled = signal.cancelled();
    tokio::pin!(cancelled);
    if signal.is_cancelled() {
        return;
    }
    cancelled.await;
}

async fn observe_process_exit(
    signal: &CancellationToken,
    ledger: &CommandExecutionLedger,
    process_id: u32,
    command_execution_id: CommandExecutionId,
    parent_tool_execution_id: &ToolExecutionId,
    exit_code: i32,
) -> CompletionApplyResult {
    wait_for_sticky_lifecycle_signal(signal).await;
    ledger
        .mark_process_exited(
            process_id,
            command_execution_id,
            parent_tool_execution_id,
            exit_code,
        )
        .await
}

pub(super) async fn wait_for_process_output_drain(signal: &CancellationToken) {
    wait_for_sticky_lifecycle_signal(signal).await;
}

pub(super) async fn wait_for_process_output_finalization(
    output_drained: &CancellationToken,
    output_closed: &AtomicBool,
    output_closed_notify: &Notify,
) {
    wait_for_process_output_drain(output_drained).await;

    let closed = output_closed_notify.notified();
    tokio::pin!(closed);
    closed.as_mut().enable();
    if output_closed.load(Ordering::Acquire) {
        return;
    }
    closed.await;
}

pub(super) async fn wait_for_process_output_for_result(
    direct_runtime: bool,
    output_drained: &CancellationToken,
    output_closed: &AtomicBool,
    output_closed_notify: &Notify,
) {
    if direct_runtime {
        wait_for_process_output_drain(output_drained).await;
    } else {
        wait_for_process_output_finalization(output_drained, output_closed, output_closed_notify)
            .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_process_terminal_event(
    process: &Arc<UnifiedExecProcess>,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    call_id: &str,
    command: &[String],
    cwd: &PathUri,
    environment_id: &str,
    process_id: u32,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    failure_message: Option<&str>,
    exit_code: i32,
    timed_out: bool,
    duration: Duration,
    tracker: Option<&SharedTurnDiffTracker>,
) {
    let process_output = Some(process.snapshot_completion_output().await);
    if let Some(message) = failure_message {
        emit_failed_exec_end_for_unified_exec(
            Arc::clone(session_ref),
            Arc::clone(turn_ref),
            call_id.to_string(),
            command.to_vec(),
            cwd.clone(),
            environment_id.to_string(),
            Some(process_id.to_string()),
            Arc::clone(transcript),
            String::new(),
            process_output,
            message.to_string(),
            timed_out,
            duration,
            tracker.cloned(),
        )
        .await;
    } else {
        emit_exec_end_for_unified_exec(
            Arc::clone(session_ref),
            Arc::clone(turn_ref),
            call_id.to_string(),
            command.to_vec(),
            cwd.clone(),
            environment_id.to_string(),
            Some(process_id.to_string()),
            Arc::clone(transcript),
            String::new(),
            process_output,
            exit_code,
            timed_out,
            duration,
            tracker.cloned(),
        )
        .await;
    }
}

impl Drop for ProcessOutputWaiterGuard {
    fn drop(&mut self) {
        self.0.adjust_process_output_waiters(-1);
    }
}

/// Spawn a background watcher that waits for the PTY to exit and then emits a
/// single ExecCommandEnd event with the aggregated transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exit_watcher(
    process: Arc<UnifiedExecProcess>,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    environment_id: String,
    process_id: u32,
    command_execution_id: CommandExecutionId,
    parent_tool_execution_id: ToolExecutionId,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    tracker: Option<SharedTurnDiffTracker>,
    known_delta: Option<PreparedKnownDelta>,
    known_delta_executor_started_at: Option<Instant>,
    tool_dispatch_timing: Option<Arc<ToolDispatchTiming>>,
) {
    let exit_token = process.cancellation_token();
    let output_drained = process.output_drained_token();
    tokio::spawn(async move {
        turn_ref.turn_timing_state.record_next_sample_block_reason(
            codex_protocol::protocol::NextSampleBlockReason::WaitingForProcessCleanup,
        );
        let exit_wait_started_at_ms = turn_ref.turn_timing_state.monotonic_offset_ms();
        wait_for_sticky_lifecycle_signal(&exit_token).await;
        let exit_observed_at = Instant::now();
        let duration = exit_observed_at.saturating_duration_since(started_at);
        let failure_message = process.failure_message();
        let exit_code = if failure_message.is_some() {
            -1
        } else {
            process.exit_code().unwrap_or(-1)
        };
        if let Some(timing) = tool_dispatch_timing.as_ref() {
            timing.mark_exec_process_exited();
            timing.record_timer_wait(ToolLifecycleTimerWait {
                wait_kind: "process_exit".to_string(),
                effective_timeout_ms: Some(
                    turn_ref
                        .turn_timing_state
                        .monotonic_offset_ms()
                        .saturating_sub(exit_wait_started_at_ms),
                ),
                wake_reason: ToolLifecycleWakeReason::Completed,
                ..Default::default()
            });
            turn_ref
                .turn_timing_state
                .record_background_tool_process_exit(&call_id, timing.snapshot(exit_observed_at));
        }
        let completion = observe_process_exit(
            &exit_token,
            &session_ref.services.command_execution,
            process_id,
            command_execution_id,
            &parent_tool_execution_id,
            exit_code,
        )
        .await;
        let tracked = matches!(
            completion,
            CompletionApplyResult::Applied | CompletionApplyResult::AlreadyApplied
        );
        debug_assert!(
            !matches!(completion, CompletionApplyResult::Stale),
            "stale process completion cannot own live command state"
        );
        let output_waiter_guard = ProcessOutputWaiterGuard::new(&turn_ref.turn_timing_state);
        turn_ref.turn_timing_state.record_next_sample_block_reason(
            codex_protocol::protocol::NextSampleBlockReason::WaitingForProcessCleanup,
        );
        let output_wait_started_at_ms = turn_ref.turn_timing_state.monotonic_offset_ms();
        let output_handles = process.output_handles();
        let direct_runtime = turn_ref.config.features.enabled(Feature::DirectRuntime);
        wait_for_process_output_for_result(
            direct_runtime,
            &output_drained,
            output_handles.output_closed.as_ref(),
            output_handles.output_closed_notify.as_ref(),
        )
        .await;
        drop(output_waiter_guard);
        if let Some(timing) = tool_dispatch_timing.as_ref() {
            timing.record_timer_wait(ToolLifecycleTimerWait {
                wait_kind: "output_drain".to_string(),
                effective_timeout_ms: Some(
                    turn_ref
                        .turn_timing_state
                        .monotonic_offset_ms()
                        .saturating_sub(output_wait_started_at_ms),
                ),
                wake_reason: ToolLifecycleWakeReason::Completed,
                ..Default::default()
            });
        }

        let mut delivered_at = None;
        if direct_runtime {
            emit_process_terminal_event(
                &process,
                &session_ref,
                &turn_ref,
                &call_id,
                &command,
                &cwd,
                &environment_id,
                process_id,
                &transcript,
                failure_message.as_deref(),
                exit_code,
                false,
                duration,
                tracker.as_ref(),
            )
            .await;
            delivered_at = Some(Instant::now());

            wait_for_process_output_finalization(
                &output_drained,
                output_handles.output_closed.as_ref(),
                output_handles.output_closed_notify.as_ref(),
            )
            .await;
        }

        if !tracked {
            tracing::debug!(
                process_id,
                "background command bookkeeping was already released"
            );
        }

        if let Some(known_delta) = known_delta.as_ref() {
            let completion_output = process.snapshot_completion_output().await;
            record_known_delta_from_process_output(
                turn_ref.config.codex_home.as_path(),
                known_delta,
                &completion_output,
                failure_message.is_none() && exit_code == 0 && !process.termination_was_requested(),
                known_delta_executor_started_at
                    .map(|started_at| Instant::now().saturating_duration_since(started_at))
                    .unwrap_or(duration),
            )
            .await;
        }

        if let Some(mut finalized_artifact) = process.raw_output_artifact().await {
            if let Some(message) = failure_message.as_ref() {
                let separator = if matches!(
                    finalized_artifact,
                    crate::tools::command_output_artifact::RawOutputArtifact::Stored {
                        bytes: 0,
                        ..
                    }
                ) {
                    ""
                } else {
                    "\n"
                };
                finalized_artifact = append_raw_output_artifact(
                    &finalized_artifact,
                    format!("{separator}{message}").as_bytes(),
                )
                .await;
            }
            session_ref
                .services
                .command_execution
                .update_running_artifact(process_id, finalized_artifact)
                .await;
        }
        if !direct_runtime {
            emit_process_terminal_event(
                &process,
                &session_ref,
                &turn_ref,
                &call_id,
                &command,
                &cwd,
                &environment_id,
                process_id,
                &transcript,
                failure_message.as_deref(),
                exit_code,
                false,
                duration,
                tracker.as_ref(),
            )
            .await;
            delivered_at = Some(Instant::now());
        }
        let finalization = session_ref
            .services
            .command_execution
            .retire_completed_process(command_execution_id, &parent_tool_execution_id)
            .await;
        reconcile_turn_progress_event(&turn_ref.turn_timing_state, 0, "background process cleanup");
        if tracked
            && !matches!(
                finalization,
                CompletionApplyResult::Applied | CompletionApplyResult::AlreadyApplied
            )
        {
            tracing::debug!(
                process_id,
                "completed command bookkeeping was already released after delivery"
            );
        }

        if let Some(tracker) = tracker.as_ref() {
            let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
            session_ref
                .services
                .command_execution
                .observe_repository_revision(&turn_ref.sub_id, observed_mutation_revision)
                .await;
        }
        let delivered_at = delivered_at.unwrap_or_else(Instant::now);
        let lifecycle = tool_dispatch_timing
            .as_ref()
            .map(|timing| timing.snapshot(delivered_at));
        tracing::info!(
            event.name = "codex.exec_command.background_lifecycle",
            conversation.id = %session_ref.thread_id,
            turn_id = %turn_ref.sub_id,
            call_id,
            process_id,
            terminal_event_delivered = true,
            request_to_spawn_ms = lifecycle
                .as_ref()
                .and_then(|snapshot| snapshot.exec_request_to_spawn_ms)
                .unwrap_or(0),
            spawn_to_exit_ms = lifecycle
                .as_ref()
                .and_then(|snapshot| snapshot.exec_spawn_to_exit_ms)
                .unwrap_or_else(|| u64::try_from(
                    exit_observed_at.saturating_duration_since(started_at).as_millis()
                ).unwrap_or(u64::MAX)),
            exit_to_delivery_ms = lifecycle
                .as_ref()
                .and_then(|snapshot| snapshot.exec_exit_to_delivery_ms)
                .unwrap_or_else(|| u64::try_from(
                    delivered_at
                        .saturating_duration_since(exit_observed_at)
                        .as_millis()
                ).unwrap_or(u64::MAX)),
            "background exec lifecycle finalized"
        );
    });
}

#[cfg(test)]
pub(crate) async fn record_known_delta_from_transcript(
    codex_home: &std::path::Path,
    prepared: &PreparedKnownDelta,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    success: bool,
    executor_cost: Duration,
) {
    let exact_output = {
        let transcript = transcript.lock().await;
        (transcript.omitted_bytes() == 0 && transcript.lagged_chunks() == 0)
            .then(|| transcript.to_bytes())
    };
    let observation = match (exact_output.as_deref(), success) {
        (Some(output), true) => KnownDeltaExecutionObservation::CompleteSuccess {
            output,
            executor_cost,
        },
        (Some(_), false) => KnownDeltaExecutionObservation::CompleteFailure,
        (None, _) => KnownDeltaExecutionObservation::Incomplete,
    };
    known_delta_store::record_execution(codex_home, prepared, observation).await;
}

pub(crate) async fn record_known_delta_from_process_output(
    codex_home: &std::path::Path,
    prepared: &PreparedKnownDelta,
    output: &ProcessOutputSnapshot,
    success: bool,
    executor_cost: Duration,
) {
    let exact_output = output
        .aggregated_output_is_exact
        .then_some(output.aggregated_output.as_slice());
    let observation = match (exact_output, success) {
        (Some(output), true) => KnownDeltaExecutionObservation::CompleteSuccess {
            output,
            executor_cost,
        },
        (Some(_), false) => KnownDeltaExecutionObservation::CompleteFailure,
        (None, _) => KnownDeltaExecutionObservation::Incomplete,
    };
    known_delta_store::record_execution(codex_home, prepared, observation).await;
}

#[allow(clippy::too_many_arguments)]
async fn drain_queued_output(
    receiver: &mut tokio::sync::broadcast::Receiver<ProcessOutputChunk>,
    pending: &mut PendingOutput,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
) {
    use tokio::sync::broadcast::error::TryRecvError;

    loop {
        match receiver.try_recv() {
            Ok(chunk) => {
                process_chunk(
                    pending,
                    transcript,
                    call_id,
                    session_ref,
                    turn_ref,
                    emitted_deltas,
                    chunk,
                )
                .await;
            }
            Err(TryRecvError::Lagged(skipped)) => {
                handle_lagged_output(
                    skipped,
                    pending,
                    transcript,
                    call_id,
                    session_ref,
                    turn_ref,
                    emitted_deltas,
                )
                .await;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_lagged_output(
    skipped: u64,
    pending: &mut PendingOutput,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
) {
    // A lag creates a gap in the byte stream, so an incomplete code point cannot be completed by
    // a later received chunk.
    flush_pending(
        pending,
        transcript,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
    )
    .await;
    {
        let mut guard = transcript.lock().await;
        guard.record_lagged_chunks(skipped);
    }
    emit_output_delta(
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        ExecOutputStream::Stdout,
        lagged_output_marker(skipped),
    )
    .await;
}

async fn process_chunk(
    pending: &mut PendingOutput,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    chunk: ProcessOutputChunk,
) {
    transcript.lock().await.push_chunk(chunk.bytes.clone());
    let stream = chunk.stream;
    let pending = match &stream {
        ExecOutputStream::Stdout => &mut pending.stdout,
        ExecOutputStream::Stderr => &mut pending.stderr,
    };
    pending.extend_from_slice(&chunk.bytes);
    emit_pending(
        pending,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        stream.clone(),
        false,
    )
    .await;
}

async fn flush_pending(
    pending: &mut PendingOutput,
    _transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
) {
    emit_pending(
        &mut pending.stdout,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        ExecOutputStream::Stdout,
        true,
    )
    .await;
    emit_pending(
        &mut pending.stderr,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        ExecOutputStream::Stderr,
        true,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn emit_pending(
    pending: &mut Vec<u8>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    stream: ExecOutputStream,
    flush_incomplete: bool,
) {
    while let Some(prefix) = split_valid_utf8_prefix_with_max(
        pending,
        UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES,
        flush_incomplete,
    ) {
        emit_output_delta(
            call_id,
            session_ref,
            turn_ref,
            emitted_deltas,
            stream.clone(),
            prefix,
        )
        .await;
    }
}

async fn emit_output_delta(
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    stream: ExecOutputStream,
    chunk: Vec<u8>,
) {
    let chunk = match emitted_deltas.claim() {
        OutputDeltaDecision::Emit => chunk,
        OutputDeltaDecision::EmitCapNotice => EXEC_OUTPUT_DELTA_CAP_NOTICE.to_vec(),
        OutputDeltaDecision::Suppress => return,
    };

    let event = ExecCommandOutputDeltaEvent {
        call_id: call_id.to_string(),
        stream,
        chunk,
    };
    session_ref.try_send_live_event(Event {
        id: turn_ref.sub_id.clone(),
        msg: EventMsg::ExecCommandOutputDelta(event),
    });
}

#[derive(Default)]
struct PendingOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Emit an ExecCommandEnd event for a unified exec session, using the transcript
/// as the primary source of aggregated_output and falling back to the provided
/// text when the transcript is empty.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    environment_id: String,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    process_output: Option<ProcessOutputSnapshot>,
    exit_code: i32,
    timed_out: bool,
    duration: Duration,
    tracker: Option<SharedTurnDiffTracker>,
) {
    let (aggregated_output, stdout, stderr) = if let Some(output) = process_output {
        (
            String::from_utf8_lossy(&output.aggregated_output).into_owned(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    } else {
        let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
        (aggregated_output.clone(), aggregated_output, String::new())
    };
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(stderr),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        tracker.as_ref(),
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        environment_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Success {
                output,
                applied_patch_delta: None,
                formatted_output: None,
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_failed_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: PathUri,
    environment_id: String,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    process_output: Option<ProcessOutputSnapshot>,
    message: String,
    timed_out: bool,
    duration: Duration,
    tracker: Option<SharedTurnDiffTracker>,
) {
    let (stdout, process_stderr, process_aggregated_output) = if let Some(output) = process_output {
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            String::from_utf8_lossy(&output.aggregated_output).into_owned(),
        )
    } else {
        let stdout = if fallback_output.is_empty() {
            resolve_aggregated_output(&transcript, fallback_output).await
        } else {
            let guard = transcript.lock().await;
            let omitted_bytes = guard.omitted_bytes();
            let lagged_chunks = guard.lagged_chunks();
            drop(guard);
            append_output_loss_markers(fallback_output, omitted_bytes, lagged_chunks)
        };
        (stdout.clone(), String::new(), stdout)
    };
    let aggregated_output = append_failure_message(process_aggregated_output, &message);
    let stderr = if process_stderr.is_empty() {
        message
    } else {
        format!("{process_stderr}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(stderr),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        tracker.as_ref(),
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
        environment_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Failure(ToolEventFailure::Output {
                output,
                formatted_output: None,
            }),
        )
        .await;
}

fn split_valid_utf8_prefix_with_max(
    buffer: &mut Vec<u8>,
    max_bytes: usize,
    flush_incomplete: bool,
) -> Option<Vec<u8>> {
    if buffer.is_empty() || max_bytes == 0 {
        return None;
    }

    let max_len = buffer.len().min(max_bytes);
    let split = match std::str::from_utf8(&buffer[..max_len]) {
        Ok(_) => max_len,
        Err(error) => {
            let valid_up_to = error.valid_up_to();
            if valid_up_to > 0 {
                valid_up_to
            } else if error.error_len().is_some() || flush_incomplete {
                // Definitively invalid bytes must make progress immediately. At the
                // end of the stream, treat a permanently incomplete sequence the
                // same way so every received byte is emitted exactly once.
                1
            } else {
                return None;
            }
        }
    };

    Some(buffer.drain(..split).collect())
}

pub(super) async fn resolve_aggregated_output(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    let guard = transcript.lock().await;
    let omitted_bytes = guard.omitted_bytes();
    let retained = if omitted_bytes == 0 {
        guard.to_bytes()
    } else {
        guard.to_bytes_with_omission_marker(&omitted_output_marker(omitted_bytes))
    };
    let lagged_chunks = guard.lagged_chunks();
    drop(guard);

    let aggregated_output = if retained.is_empty() {
        fallback
    } else {
        String::from_utf8_lossy(&retained).to_string()
    };
    append_output_loss_markers(aggregated_output, omitted_bytes, lagged_chunks)
}

fn append_output_loss_markers(
    mut output: String,
    omitted_bytes: usize,
    lagged_chunks: u64,
) -> String {
    if omitted_bytes > 0 {
        let marker = String::from_utf8_lossy(&omitted_output_marker(omitted_bytes)).into_owned();
        if !output.contains(marker.as_str()) {
            output.push_str(&marker);
        }
    }
    if lagged_chunks > 0 {
        let marker = String::from_utf8_lossy(&lagged_output_marker(lagged_chunks)).into_owned();
        if !output.contains(marker.as_str()) {
            output.push_str(&marker);
        }
    }
    output
}

fn append_failure_message(mut output: String, message: &str) -> String {
    if output.is_empty() {
        return message.to_string();
    }
    output.push('\n');
    output.push_str(message);
    output
}

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
