use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::UnifiedExecContext;
use super::UnifiedExecError;
pub(super) use super::head_tail_buffer::omitted_output_marker;
use super::process::UnifiedExecProcess;
use crate::exec::EXEC_OUTPUT_DELTA_CAP_NOTICE;
use crate::exec::OutputDeltaDecision;
use crate::exec::OutputDeltaLimiter;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::command_output_artifact::append_raw_output_artifact;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::tools::known_delta_store;
use crate::tools::known_delta_store::KnownDeltaExecutionObservation;
use crate::tools::known_delta_store::PreparedKnownDelta;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
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

        let mut pending = Vec::<u8>::new();
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
    process_id: i32,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    started_at: Instant,
    tracker: Option<SharedTurnDiffTracker>,
    validation_observation: Option<crate::validation_admission::ValidationObservationToken>,
    validation_leader: Option<crate::validation_admission::ValidationLeaderOwnership>,
    validation_waiter: Option<crate::validation_admission::ValidationLeader>,
    known_delta: Option<PreparedKnownDelta>,
    known_delta_executor_started_at: Option<Instant>,
) {
    let exit_token = process.cancellation_token();
    let output_drained = process.output_drained_token();
    if let Some(leader) = validation_leader.as_ref() {
        let validation_cancellation = leader.cancellation_token();
        let process_exit = exit_token.clone();
        let validation_process = Arc::clone(&process);
        tokio::spawn(async move {
            tokio::select! {
                _ = validation_cancellation.cancelled() => {
                    if !validation_process.has_exited()
                        && let Err(error) = validation_process.terminate_confirmed().await
                    {
                        tracing::warn!(%error, "failed to terminate abandoned shared validation");
                    }
                }
                _ = process_exit.cancelled() => {}
            }
        });
    }

    tokio::spawn(async move {
        let _validation_waiter = validation_waiter;
        exit_token.cancelled().await;
        output_drained.cancelled().await;

        let duration = Instant::now().saturating_duration_since(started_at);
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        if let Some(observation) = validation_observation {
            if process.termination_was_requested() {
                observation.record_cancelled(duration_ms).await;
            } else {
                observation.record_completed(duration_ms).await;
            }
        }
        let failure_message = process.failure_message();
        let exit_code = if failure_message.is_some() {
            -1
        } else {
            process.exit_code().unwrap_or(-1)
        };

        // Record the process exit before event delivery. The end event below independently marks
        // possible workspace mutation, so missing command bookkeeping cannot turn a completed
        // process into a tool failure.
        let tracked = session_ref
            .services
            .command_execution
            .mark_running_process_completed(process_id, exit_code)
            .await;
        if !tracked {
            tracing::debug!(
                process_id,
                "background command bookkeeping was already released"
            );
        }

        if let Some(known_delta) = known_delta.as_ref() {
            record_known_delta_from_transcript(
                turn_ref.config.codex_home.as_path(),
                known_delta,
                &transcript,
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

        if let Some(message) = failure_message.as_ref() {
            emit_failed_exec_end_for_unified_exec(
                Arc::clone(&session_ref),
                Arc::clone(&turn_ref),
                call_id.clone(),
                command.clone(),
                cwd.clone(),
                environment_id.clone(),
                Some(process_id.to_string()),
                Arc::clone(&transcript),
                String::new(),
                message.clone(),
                duration,
                tracker.clone(),
            )
            .await;
        } else {
            emit_exec_end_for_unified_exec(
                Arc::clone(&session_ref),
                Arc::clone(&turn_ref),
                call_id.clone(),
                command.clone(),
                cwd.clone(),
                environment_id.clone(),
                Some(process_id.to_string()),
                Arc::clone(&transcript),
                String::new(),
                exit_code,
                duration,
                tracker.clone(),
            )
            .await;
        }

        if let Some(tracker) = tracker.as_ref() {
            let observed_mutation_revision = tracker.lock().await.current_mutation_revision();
            session_ref
                .services
                .command_execution
                .observe_repository_revision(&turn_ref.sub_id, observed_mutation_revision)
                .await;
        }
        if let Some(leader) = validation_leader {
            let text = transcript.lock().await.to_bytes();
            leader
                .complete(crate::validation_admission::ReusableValidationResult {
                    value: serde_json::json!({
                        "text": String::from_utf8_lossy(&text),
                        "success": failure_message.is_none() && exit_code == 0,
                    }),
                })
                .await;
        }
    });
}

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

#[allow(clippy::too_many_arguments)]
async fn drain_queued_output(
    receiver: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    pending: &mut Vec<u8>,
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
    pending: &mut Vec<u8>,
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
        lagged_output_marker(skipped),
    )
    .await;
}

async fn process_chunk(
    pending: &mut Vec<u8>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    chunk: Vec<u8>,
) {
    pending.extend_from_slice(&chunk);
    emit_pending(
        pending,
        transcript,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        false,
    )
    .await;
}

async fn flush_pending(
    pending: &mut Vec<u8>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
) {
    emit_pending(
        pending,
        transcript,
        call_id,
        session_ref,
        turn_ref,
        emitted_deltas,
        true,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn emit_pending(
    pending: &mut Vec<u8>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    flush_incomplete: bool,
) {
    while let Some(prefix) = split_valid_utf8_prefix_with_max(
        pending,
        UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES,
        flush_incomplete,
    ) {
        {
            let mut guard = transcript.lock().await;
            guard.push_chunk(prefix.clone());
        }
        emit_output_delta(call_id, session_ref, turn_ref, emitted_deltas, prefix).await;
    }
}

async fn emit_output_delta(
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &OutputDeltaLimiter,
    chunk: Vec<u8>,
) {
    let chunk = match emitted_deltas.claim() {
        OutputDeltaDecision::Emit => chunk,
        OutputDeltaDecision::EmitCapNotice => EXEC_OUTPUT_DELTA_CAP_NOTICE.to_vec(),
        OutputDeltaDecision::Suppress => return,
    };

    let event = ExecCommandOutputDeltaEvent {
        call_id: call_id.to_string(),
        stream: ExecOutputStream::Stdout,
        chunk,
    };
    session_ref
        .send_event(turn_ref.as_ref(), EventMsg::ExecCommandOutputDelta(event))
        .await;
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
    exit_code: i32,
    duration: Duration,
    tracker: Option<SharedTurnDiffTracker>,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
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
    message: String,
    duration: Duration,
    tracker: Option<SharedTurnDiffTracker>,
) {
    let stdout = if fallback_output.is_empty() {
        resolve_aggregated_output(&transcript, fallback_output).await
    } else {
        let guard = transcript.lock().await;
        let omitted_bytes = guard.omitted_bytes();
        let lagged_chunks = guard.lagged_chunks();
        drop(guard);
        append_output_loss_markers(fallback_output, omitted_bytes, lagged_chunks)
    };
    let aggregated_output = if stdout.is_empty() {
        message.clone()
    } else {
        format!("{stdout}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(message),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
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
                applied_patch_delta: None,
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

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
