use std::sync::Arc;
use std::time::Duration;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::compaction_status_from_result;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote::process_compacted_history_with_retained_input;
use crate::hook_runtime::run_post_compact_hook_gate;
use crate::hook_runtime::run_pre_compact_hook_gate;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::turn_timing::TurnTimingState;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WarningEvent;
#[cfg(test)]
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

#[path = "compact_remote_v2_trace.rs"]
mod trace;
use trace::AsyncCompactionTraceContext;

// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;

// The provider returns the checkpoint only when generation finishes, so the whole
// generation is silence to the progress deadline. Long tasks grow the summary; at
// about 14K tokens it needed ~300 s and the default 300 s deadline failed every attempt.
const REMOTE_COMPACTION_V2_PROGRESS_TIMEOUT: Duration = Duration::from_secs(15 * 60);

fn remote_compaction_progress_timeout(provider: &ModelProviderInfo) -> Duration {
    provider
        .stream_idle_timeout()
        .max(REMOTE_COMPACTION_V2_PROGRESS_TIMEOUT)
}

fn preserve_model_fallback_failure(
    previous_error: &CodexErr,
    final_error: CodexErr,
) -> (WarningEvent, CodexErr) {
    let warning = WarningEvent {
        message: format!(
            "Remote compaction failed with the previous model: {previous_error}; retry with the current model also failed: {final_error}"
        ),
    };
    (warning, final_error)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    client_session: &mut ModelClientSession,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        reason,
        CompactionImplementation::ResponsesCompactionV2,
        phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        fallback_step_context.as_ref(),
        Some(client_session),
        initial_context_injection,
        compaction_metadata,
        emit_error_event,
        cancellation_token,
    )
    .await
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    // Standalone compaction is its own request boundary, so it captures a fresh step.
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await?;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompactionV2,
        CompactionPhase::StandaloneTurn,
    );
    let world_state = Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
    let initial_context_injection = InitialContextInjection::AtStart(world_state);
    let result = run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*client_session*/ None,
        initial_context_injection.clone(),
        compaction_metadata,
        // A recoverable rejection must not mark a continuing manual turn failed.
        /*emit_error_event*/
        false,
        cancellation_token,
    )
    .await;
    let error = match result {
        Ok(()) => return Ok(()),
        Err(error @ CodexErr::TurnAborted) => return Err(error),
        Err(error) => error,
    };
    if !crate::compact_model_fallback::should_retry_with_local_compaction(&error) {
        sess.send_event(
            &turn_context,
            EventMsg::Error(
                error.to_error_event(Some("Error running remote compact task".to_string())),
            ),
        )
        .await;
        return Err(error);
    }
    if let Some(advice) = error.retry_after() {
        let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        crate::responses_retry::wait_for_retry_deadline(advice.deadline(), cancellation_token)
            .await?;
    }
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    sess.send_event(
        &turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!("Remote compaction failed: {error}. Compacting locally instead."),
        }),
    )
    .await;
    crate::tasks::emit_compact_metric(
        &sess.services.session_telemetry,
        "local",
        /*manual*/ true,
    );
    let input = vec![codex_protocol::user_input::UserInput::Text {
        text: turn_context
            .config
            .compact_prompt
            .as_deref()
            .unwrap_or(crate::compact::SUMMARIZATION_PROMPT)
            .to_string(),
        text_elements: Vec::new(),
    }];
    // The existing local compactor owns source recovery and atomic publication.
    // Try it once; storage, credentials and cancellation never enter this route.
    crate::compact::run_compact_task_with_initial_context(
        sess,
        turn_context,
        input,
        initial_context_injection,
        cancellation_token,
    )
    .await
}

/// `emit_error_event` is false when the caller recovers from the failure; a
/// turn error event would mark a turn that continues as failed.
#[allow(clippy::too_many_arguments)]
async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    emit_error_event: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let trigger = compaction_metadata.trigger();
    let reason = compaction_metadata.reason();
    let implementation = compaction_metadata.implementation();
    let phase = compaction_metadata.phase();
    let mut analytics_details = CompactionAnalyticsDetails {
        active_context_tokens_before: Some(sess.get_total_token_usage().await),
        ..Default::default()
    };
    let attempt = CompactionAnalyticsAttempt::begin(
        sess.as_ref(),
        turn_context.as_ref(),
        trigger,
        reason,
        implementation,
        phase,
    )
    .await;
    if run_pre_compact_hook_gate(sess, turn_context, trigger).await {
        let error = CodexErr::TurnAborted;
        attempt
            .track(
                sess.as_ref(),
                codex_analytics::CompactionStatus::Interrupted,
                Some(&error),
                analytics_details,
            )
            .await;
        return Err(error);
    }
    let mut context_compaction_item = ContextCompactionItem::new();
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item.clone());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let result = run_remote_compact_task_inner_impl(
        sess,
        step_context,
        fallback_step_context,
        client_session,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
        &mut context_compaction_item,
        cancellation_token,
    )
    .await;
    sess.emit_turn_item_completed_with_legacy_events(
        turn_context,
        TurnItem::ContextCompaction(context_compaction_item),
        result.is_ok(),
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if result.is_ok() && run_post_compact_hook_gate(sess, turn_context, trigger, None).await {
        attempt
            .track(sess.as_ref(), status, codex_error, analytics_details)
            .await;
        return Err(CodexErr::TurnAborted);
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    match result {
        Ok(()) => Ok(()),
        Err(err @ CodexErr::TurnAborted) => Err(err),
        Err(err) => {
            sess.track_turn_codex_error(turn_context, &err);
            if emit_error_event {
                let event = EventMsg::Error(
                    err.to_error_event(Some("Error running remote compact task".to_string())),
                );
                sess.send_event(turn_context, event).await;
            }
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    mut client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    compaction_item: &mut ContextCompactionItem,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let compaction_trace = AsyncCompactionTraceContext::new(
        &sess.terminal_tasks,
        sess.services.rollout_thread_trace.compaction_trace_context(
            turn_context.sub_id.as_str(),
            &compaction_item.id,
            turn_context.model_info.slug.as_str(),
            turn_context.provider.info().name.as_str(),
        ),
    );

    // Each attempt discounts its own trimming from this pre-compaction baseline.
    let active_context_tokens_before = analytics_details.active_context_tokens_before;
    let attempt = run_remote_compact_v2_attempt(
        sess,
        step_context,
        client_session.as_deref_mut(),
        &compaction_trace,
        compaction_metadata,
        analytics_details,
        cancellation_token,
    )
    .await;
    let (attempt, compaction_turn_context) = match attempt {
        Ok(attempt) => (attempt, turn_context),
        Err(error) => {
            let Some(fallback_step_context) = fallback_step_context else {
                return Err(error);
            };
            if !should_retry_with_current_model(&error) {
                return Err(error);
            }
            if let Some(advice) = error.retry_after() {
                let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
                crate::responses_retry::wait_for_retry_deadline(
                    advice.deadline(),
                    cancellation_token,
                )
                .await?;
            }
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace = compaction_trace.with_context(
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    &compaction_item.id,
                    fallback_turn_context.model_info.slug.as_str(),
                    fallback_turn_context.provider.info().name.as_str(),
                ),
            );
            analytics_details.active_context_tokens_before = active_context_tokens_before;
            let fallback_result = run_remote_compact_v2_attempt(
                sess,
                fallback_step_context,
                client_session.as_deref_mut(),
                &fallback_compaction_trace,
                compaction_metadata,
                analytics_details,
                cancellation_token,
            )
            .await;
            record_model_fallback(
                &sess.services.session_telemetry,
                turn_context.model_info.slug.as_str(),
                fallback_turn_context.model_info.slug.as_str(),
                compaction_metadata.reason(),
                compaction_metadata.implementation(),
                fallback_result.as_ref().err(),
            );
            match fallback_result {
                Ok(attempt) => (attempt, fallback_turn_context),
                Err(fallback_error) => {
                    let (warning, fallback_error) =
                        preserve_model_fallback_failure(&error, fallback_error);
                    sess.send_event(fallback_turn_context, EventMsg::Warning(warning))
                        .await;
                    return Err(fallback_error);
                }
            }
        }
    };
    let RemoteCompactV2Attempt {
        trace_input_history,
        retained_input,
        compaction_output,
        token_usage,
        owned_client_session: _owned_client_session,
        ..
    } = attempt;
    if let Some(token_usage) = token_usage.as_ref() {
        analytics_details.record_compaction_receipt(token_usage);
    }
    let retained_images = retained_input.retained_images;
    let omitted_images = retained_input.omitted_images;
    let omitted_text = retained_input.omitted_text;
    // The common installer refreshes a plan fallback only when its actual
    // rendered initial context lacks authoritative task state.
    analytics_details.retained_image_count = Some(retained_images);
    let (new_history, world_state_baseline, fragment_digests) =
        process_compacted_history_with_retained_input(
            sess.as_ref(),
            compaction_turn_context.as_ref(),
            vec![compaction_output],
            retained_input.items,
            &initial_context_injection,
        )
        .await?;

    let reference_context_item = match &initial_context_injection {
        #[cfg(test)]
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::AtStart(_) => {
            Some(compaction_turn_context.to_turn_context_item_async().await)
        }
    };
    let compacted_item = persisted_v2_compacted_item(new_history.clone());
    let trace_replacement_history = trace_input_history.as_ref().map(|_| new_history.clone());
    sess.replace_compacted_history(
        compaction_turn_context,
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
        compacted_item,
    )
    .await?;
    if let (Some(trace_input_history), Some(replacement_history)) =
        (trace_input_history, trace_replacement_history)
    {
        compaction_trace
            .record_installed(trace_input_history, replacement_history)
            .await;
    }
    if let Some(client_session) = client_session {
        // Refreshed instructions and local recovery/pin metadata were not part of
        // the provider's response. Send the complete installed checkpoint once;
        // never mark that locally injected content as already inherited.
        client_session.invalidate_provider_history_inheritance("installed remote checkpoint");
    }
    sess.recompute_token_usage(compaction_turn_context).await;

    compaction_item.tokens_before = analytics_details.active_context_tokens_before;
    compaction_item.tokens_after = Some(sess.get_total_token_usage().await);
    compaction_item.retained_image_count = Some(retained_images);
    compaction_item.omitted_image_count = Some(omitted_images);
    compaction_item.omitted_text = Some(omitted_text);

    Ok(())
}

struct RemoteCompactionV2Output {
    compaction_output: ResponseItem,
    token_usage: Option<TokenUsage>,
}

#[derive(serde::Serialize)]
struct RemoteCompactionV2TraceRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    parallel_tool_calls: bool,
}

async fn run_remote_compaction_request_v2(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
    compaction_trace: &AsyncCompactionTraceContext,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactionV2Output> {
    client_session.set_turn_timing(Arc::clone(&turn_context.turn_timing_state));
    client_session.set_startup_timing(Arc::clone(&sess.startup_timing));
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retry_state = ResponsesStreamRetryState::default();
    turn_context.turn_timing_state.begin_compaction_generation();
    loop {
        let attempt_started = tokio::time::Instant::now();
        let trace_owner = if compaction_trace.is_enabled() {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                owner = compaction_trace.start_attempt(
                    turn_context.model_info.slug.clone(),
                    prompt.base_instructions.text.clone(),
                    Arc::clone(&prompt.input),
                    prompt.parallel_tool_calls,
                ) => owner,
            }
        } else {
            None
        };
        let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
        let inference_trace_context = InferenceTraceContext::disabled();
        let service_tier =
            crate::session::turn::service_tier_for_sampling(sess, turn_context).await;
        let stream_result = tokio::select! {
            _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
            result = client_session.stream(
                prompt,
                &turn_context.model_info,
                &turn_context.session_telemetry,
                turn_context.request_reasoning_effort(),
                codex_protocol::config_types::ReasoningSummary::None,
                service_tier,
                responses_metadata,
                &inference_trace_context,
            ) => result,
        };
        drop(model_request_timing_guard);
        let mut terminal_receipt = None;
        let result = match stream_result {
            Ok(stream) => {
                collect_compaction_output_with_receipt(
                    stream,
                    Some(&turn_context.turn_timing_state),
                    Some((sess, turn_context)),
                    cancellation_token,
                    &mut terminal_receipt,
                )
                .await
            }
            Err(err) => Err(err),
        };
        // Transfer terminal ownership before optional persistence. A forced
        // drop while the terminal write is queued must not race it with a
        // fabricated physical cancellation.
        if let Some(trace_owner) = trace_owner {
            let cancelled = matches!(&result, Err(CodexErr::TurnAborted));
            let trace_result = result
                .as_ref()
                .map(|output| vec![output.compaction_output.clone()])
                .map_err(ToString::to_string);
            trace_owner.record_result(trace_result, terminal_receipt, cancelled);
        }

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                // Do not restart a fully consumed progress budget. The caller
                // owns exact-output retirement/local compaction recovery; short
                // transient failures still use the transport retry policy below.
                if prompt
                    .stream_progress_timeout
                    .is_some_and(|timeout| attempt_started.elapsed() >= timeout)
                {
                    return Err(err);
                }
                handle_retryable_response_stream_error(
                    &mut retry_state,
                    max_retries,
                    err,
                    client_session,
                    sess,
                    turn_context,
                    ResponsesStreamRequest::RemoteCompactionV2,
                    cancellation_token,
                )
                .await?;
                turn_context.turn_timing_state.record_model_retry();
            }
        }
    }
}

/// Test convenience; production retains the terminal receipt for its trace.
#[cfg(test)]
async fn collect_compaction_output(
    stream: ResponseStream,
    timing_state: Option<&Arc<TurnTimingState>>,
    accounting_context: Option<(&Session, &TurnContext)>,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactionV2Output> {
    collect_compaction_output_with_receipt(
        stream,
        timing_state,
        accounting_context,
        cancellation_token,
        &mut None,
    )
    .await
}

async fn collect_compaction_output_with_receipt(
    mut stream: ResponseStream,
    timing_state: Option<&Arc<TurnTimingState>>,
    accounting_context: Option<(&Session, &TurnContext)>,
    cancellation_token: &CancellationToken,
    terminal_receipt: &mut Option<codex_rollout_trace::CompactionResponseReceipt>,
) -> CodexResult<RemoteCompactionV2Output> {
    let identity = stream.attempt_identity().cloned();
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut compaction_output = None;
    let mut saw_completed = false;
    let mut completed_token_usage = None;
    loop {
        let model_stream_wait_timing_guard =
            timing_state.map(super::turn_timing::TurnTimingState::begin_model_stream_wait);
        let next_event = tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            event = stream.next() => event,
        };
        drop(model_stream_wait_timing_guard);
        let Some(event) = next_event else {
            break;
        };
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                if error.terminal_usage().is_some() || error.terminal_response_id().is_some() {
                    *terminal_receipt = Some(codex_rollout_trace::CompactionResponseReceipt {
                        response_id: error.terminal_response_id().map(str::to_owned),
                        sampling_request_id: identity
                            .as_ref()
                            .map(|id| id.sampling_request_id.clone()),
                        physical_attempt_id: identity
                            .as_ref()
                            .map(|id| id.physical_attempt_id.clone()),
                        token_usage: error.terminal_usage().and_then(Clone::clone),
                        provider_completed: false,
                    });
                }
                if let Some(token_usage) = error.terminal_usage() {
                    if let Some(timing) = timing_state {
                        timing.record_generation_token_usage(token_usage.as_ref());
                    }
                    if let (Some((sess, turn)), Some(usage)) =
                        (accounting_context, token_usage.as_ref())
                    {
                        sess.record_compaction_token_usage(turn, Some(usage))
                            .await?;
                    }
                }
                return Err(error.into_response_error());
            }
        };
        let _model_stream_processing_timing_guard =
            timing_state.and_then(|timing| timing.begin_model_stream_processing(&event));
        if let Some(timing) = timing_state {
            // Ignored compactor messages are not user-visible or actionable.
            if matches!(
                &event,
                ResponseEvent::OutputItemDone(ResponseItem::Compaction { .. })
                    | ResponseEvent::Completed { .. }
            ) {
                timing.record_response_event_milestones(&event);
            }
            if let ResponseEvent::Completed { token_usage, .. } = &event {
                timing.record_generation_token_usage(token_usage.as_ref());
            }
        }
        match event {
            ResponseEvent::OutputItemDone(item) => {
                output_item_count += 1;
                if let ResponseItem::Compaction { .. } = item {
                    compaction_count += 1;
                    if compaction_output.is_none() {
                        compaction_output = Some(item);
                    }
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                // Capture before accounting or checkpoint validation can fail.
                *terminal_receipt = Some(codex_rollout_trace::CompactionResponseReceipt {
                    response_id: Some(response_id),
                    sampling_request_id: identity.as_ref().map(|id| id.sampling_request_id.clone()),
                    physical_attempt_id: identity.as_ref().map(|id| id.physical_attempt_id.clone()),
                    token_usage: token_usage.clone(),
                    provider_completed: true,
                });
                // Consumption belongs to the physical response, even when its
                // checkpoint is invalid and must never be installed.
                if let Some((sess, turn)) = accounting_context {
                    sess.record_compaction_token_usage(turn, token_usage.as_ref())
                        .await?;
                }
                saw_completed = true;
                completed_token_usage = token_usage;
                break;
            }
            ResponseEvent::RateLimits(snapshot) => {
                if let Some((sess, turn)) = accounting_context {
                    sess.update_rate_limits(turn, snapshot).await;
                }
            }
            _ => {}
        }
    }

    if !saw_completed {
        return Err(CodexErr::Stream(
            "remote compaction v2 stream closed before response.completed".to_string(),
            None,
        ));
    }

    if compaction_count != 1 {
        return Err(CodexErr::Fatal(format!(
            "remote compaction v2 expected exactly one compaction output item, got {compaction_count} from {output_item_count} output items"
        )));
    }

    let Some(compaction_output) = compaction_output else {
        unreachable!("compaction output must exist when count is exactly one");
    };
    if matches!(&compaction_output, ResponseItem::Compaction { encrypted_content, .. }
        if encrypted_content.is_empty())
    {
        return Err(CodexErr::Fatal(
            "remote compaction v2 returned an empty checkpoint".to_string(),
        ));
    }
    Ok(RemoteCompactionV2Output {
        compaction_output,
        token_usage: completed_token_usage,
    })
}

#[cfg(test)]
tokio::task_local! {
    static RETENTION_PREPARATIONS: std::cell::Cell<usize>;
}

async fn prepare_v2_retained_input_source(
    sess: &Session,
    prompt_input: &[ResponseItem],
) -> CodexResult<crate::compact::TaskInputCheckpoint> {
    #[cfg(test)]
    let _ = RETENTION_PREPARATIONS.try_with(|count| count.set(count.get() + 1));
    let mut retained = crate::compact::build_task_input_checkpoint_with_details(prompt_input);
    let omission_receipt_ids =
        crate::compact::new_omission_receipt_ids(&retained.items, prompt_input);
    if let Some(text) = crate::compact::persist_task_compaction_text_recovery(
        sess,
        prompt_input,
        retained.needs_source_recovery(),
    )
    .await?
    {
        crate::compact::bind_omission_receipts_to_recovery(
            &mut retained.items,
            &text,
            &omission_receipt_ids,
        );
        retained
            .items
            .push(crate::compact::compaction_context_message(text));
    }
    Ok(retained)
}

#[cfg(test)]
async fn prepare_v2_retained_input(
    sess: &Session,
    prompt_input: &[ResponseItem],
) -> CodexResult<(Vec<ResponseItem>, usize)> {
    let mut retained = prepare_v2_retained_input_source(sess, prompt_input).await?;
    if let Some(plan) = crate::compact::retained_plan_context(sess).await? {
        retained.items.push(plan);
    }
    Ok((retained.items, retained.retained_images))
}

#[cfg(test)]
fn build_v2_compacted_history(
    prompt_input: Vec<ResponseItem>,
    compaction_output: ResponseItem,
) -> (Vec<ResponseItem>, usize) {
    let (mut history, retained_image_count, _) =
        crate::compact::build_task_input_checkpoint(&prompt_input);
    history.push(compaction_output);
    (history, retained_image_count)
}

fn persisted_v2_compacted_item(replacement_history: Vec<ResponseItem>) -> CompactedItem {
    CompactedItem {
        message: String::new(),
        // Resume and fork reconstruction must install the same opaque checkpoint and bounded
        // unresolved user tail that became live above. `None` is reserved for legacy records.
        replacement_history: Some(replacement_history),
        // The ordered history commit assigns and publishes the next window.
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelClient;
    use crate::responses_metadata::CodexResponsesRequestKind;
    use crate::test_harness::responses;
    use codex_login::auth::AgentIdentityAuthPolicy;
    use codex_model_provider::create_model_provider;
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::MessagePhase;
    use codex_rollout_trace::ExecutionStatus;
    use codex_rollout_trace::RawTraceEventPayload;
    use codex_rollout_trace::TraceWriter;
    use codex_rollout_trace::replay_bundle;
    use codex_utils_output_truncation::approx_token_count;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn message(role: &str, text: &str, phase: Option<MessagePhase>) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn response_stream(events: Vec<CodexResult<ResponseEvent>>) -> ResponseStream {
        let (tx_event, rx_event) =
            codex_api::response_event_channel::response_event_channel(events.len().max(1));
        for event in events {
            tx_event
                .try_send(event)
                .expect("response stream test channel should have capacity");
        }
        drop(tx_event);
        ResponseStream {
            rx_event,
            attempt_identity: None,
            consumer_dropped: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn remote_compaction_uses_current_supported_service_tier() -> anyhow::Result<()> {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        let home = TempDir::new()?;
        let (session, mut turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test-key"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let turn = Arc::get_mut(&mut turn).expect("test turn is uniquely owned");
        // Deliberately stale: each request must resolve the live setting instead.
        Arc::make_mut(&mut turn.config).service_tier = Some("priority".into());
        // Compatibility compaction may retain a UI preset that is not the
        // configured intent. Wire effort must use the latter and target limits.
        use codex_protocol::openai_models::ReasoningEffort;
        use codex_protocol::openai_models::ReasoningEffortPreset;
        turn.reasoning_effort = Some(ReasoningEffort::Medium);
        turn.configured_reasoning_effort = Some(ReasoningEffort::Ultra);
        turn.model_info.supports_reasoning_summaries = true;
        turn.model_info.supported_reasoning_levels = [
            ReasoningEffort::XHigh,
            ReasoningEffort::Low,
            ReasoningEffort::High,
        ]
        .into_iter()
        .map(|effort| ReasoningEffortPreset {
            effort,
            description: String::new(),
        })
        .collect();
        let metadata = turn.turn_metadata_state.to_responses_metadata(
            session.installation_id.clone(),
            session.current_window_id().await,
            CodexResponsesRequestKind::Compaction(CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::ResponsesCompactionV2,
                CompactionPhase::StandaloneTurn,
            )),
        );
        for (fast, supported, configured, expected) in [
            (true, true, Some("priority"), Some("priority")),
            (false, true, Some("priority"), None),
            (true, false, Some("priority"), None),
            // Explicit standard routing is a config sentinel, omitted on wire.
            (true, false, Some("default"), None),
            (true, true, None, None),
        ] {
            if fast {
                Arc::make_mut(&mut turn.config)
                    .features
                    .enable(codex_features::Feature::FastMode)?;
            } else {
                Arc::make_mut(&mut turn.config)
                    .features
                    .disable(codex_features::Feature::FastMode)?;
            }
            turn.model_info.service_tiers = if supported {
                vec![codex_protocol::openai_models::ModelServiceTier {
                    id: "priority".into(),
                    name: "Priority".into(),
                    description: "Priority".into(),
                }]
            } else {
                Vec::new()
            };
            session
                .update_settings(crate::session::SessionSettingsUpdate {
                    service_tier: Some(configured.map(str::to_string)),
                    ..Default::default()
                })
                .await?;
            let log = responses::mount_sse_once(
                &server,
                responses::sse(vec![
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {"type": "compaction", "encrypted_content": "summary"},
                    }),
                    responses::ev_completed("compacted"),
                ]),
            )
            .await;
            let mut client = session.services.model_client.new_session();
            run_remote_compaction_request_v2(
                &session,
                turn,
                &mut client,
                &Prompt::default(),
                &metadata,
                &AsyncCompactionTraceContext::disabled(),
                &CancellationToken::new(),
            )
            .await?;
            assert_eq!(
                log.single_request()
                    .body_json()
                    .get("service_tier")
                    .and_then(serde_json::Value::as_str),
                expected
            );
            assert_eq!(
                log.single_request().body_json()["reasoning"]["effort"],
                "xhigh"
            );
        }
        Ok(())
    }

    #[test]
    fn remote_compaction_progress_timeout_never_shortens_provider_timeout() {
        let mut provider = ModelProviderInfo::default();
        assert_eq!(
            remote_compaction_progress_timeout(&provider),
            REMOTE_COMPACTION_V2_PROGRESS_TIMEOUT
        );
        provider.stream_idle_timeout_ms = Some(30 * 60 * 1000);
        assert_eq!(
            remote_compaction_progress_timeout(&provider),
            Duration::from_secs(30 * 60)
        );
    }

    #[tokio::test]
    async fn remote_compaction_outlasts_provider_idle_timeout() -> anyhow::Result<()> {
        crate::test_harness::require_network!();
        let server = responses::start_mock_server().await;
        // The checkpoint arrives only after longer silence than the provider allows.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(responses::sse(vec![
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "item": {"type": "compaction", "encrypted_content": "checkpoint"},
                        }),
                        responses::ev_completed("compacted"),
                    ]))
                    .set_delay(Duration::from_millis(400)),
            )
            .mount(&server)
            .await;
        let home = TempDir::new()?;
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test-key"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                    config.model_provider.stream_idle_timeout_ms = Some(100);
                    config.model_provider.stream_max_retries = Some(0);
                },
            )
            .await;

        // Control: a request without the compaction budget keeps the provider timeout.
        let metadata = turn.turn_metadata_state.to_responses_metadata(
            session.installation_id.clone(),
            session.current_window_id().await,
            CodexResponsesRequestKind::Compaction(CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::ResponsesCompactionV2,
                CompactionPhase::StandaloneTurn,
            )),
        );
        let mut client = session.services.model_client.new_session();
        let control = run_remote_compaction_request_v2(
            &session,
            &turn,
            &mut client,
            &Prompt::default(),
            &metadata,
            &AsyncCompactionTraceContext::disabled(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            control.is_err(),
            "the provider timeout must still bound other requests"
        );

        session
            .record_conversation_items(
                &turn,
                &[message("user", "compact this history", /*phase*/ None)],
            )
            .await?;
        run_remote_compact_task(
            Arc::clone(&session),
            Arc::clone(&turn),
            &CancellationToken::new(),
        )
        .await?;
        assert!(session.clone_history().await.raw_items().iter().any(|item| matches!(
            item,
            ResponseItem::Compaction { encrypted_content, .. } if encrypted_content == "checkpoint"
        )));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_compaction_returns_first_exhausted_progress_budget_to_recovery() -> anyhow::Result<()> {
        use crate::test_harness::streaming_sse::StreamingSseChunk;
        use crate::test_harness::streaming_sse::start_streaming_sse_server;

        crate::test_harness::require_network!();
        // Each response sends headers and then nothing, so every attempt uses its
        // whole progress budget. Holding the gates keeps the streams open.
        let mut gates = Vec::new();
        let mut stalled_responses = Vec::new();
        for _ in 0..3 {
            let (gate, gated) = tokio::sync::oneshot::channel::<()>();
            gates.push(gate);
            stalled_responses.push(vec![StreamingSseChunk {
                gate: Some(gated),
                body: String::new(),
            }]);
        }
        let (server, _completions) = start_streaming_sse_server(stalled_responses).await;
        let home = TempDir::new()?;
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test-key"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                    config.model_provider.stream_max_retries = Some(2);
                },
            )
            .await;
        let metadata = turn.turn_metadata_state.to_responses_metadata(
            session.installation_id.clone(),
            session.current_window_id().await,
            CodexResponsesRequestKind::Compaction(CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::ResponsesCompactionV2,
                CompactionPhase::StandaloneTurn,
            )),
        );
        let prompt = Prompt {
            stream_progress_timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let mut client = session.services.model_client.new_session();
        let result = run_remote_compaction_request_v2(
            &session,
            &turn,
            &mut client,
            &prompt,
            &metadata,
            &AsyncCompactionTraceContext::disabled(),
            &CancellationToken::new(),
        )
        .await;

        let Err(error) = result else {
            panic!("every attempt stalls");
        };
        assert!(
            error
                .to_string()
                .contains("idle timeout waiting for SSE after 200ms"),
            "{error}"
        );
        // A fresh long timeout is not a transport retry. Preserve the first
        // error for the caller's existing retirement/local fallback.
        assert_eq!(server.requests().await.len(), 1);
        drop(gates);
        server.shutdown().await;
        Ok(())
    }

    #[test]
    fn dual_model_failure_preserves_final_typed_error_and_both_diagnostics() {
        use codex_protocol::error::UsageLimitReachedError;
        use codex_protocol::protocol::CodexErrorInfo;

        let cases = [
            (
                CodexErr::ServerOverloaded { retry_after: None },
                CodexErrorInfo::ServerOverloaded,
            ),
            (
                CodexErr::UsageLimitReached(UsageLimitReachedError {
                    plan_type: None,
                    resets_at: None,
                    rate_limits: None,
                    promo_message: None,
                    rate_limit_reached_type: None,
                }),
                CodexErrorInfo::UsageLimitExceeded,
            ),
        ];

        for (final_error, expected_info) in cases {
            let final_message = final_error.to_string();
            let (warning, final_error) = preserve_model_fallback_failure(
                &CodexErr::InvalidRequest("previous-model marker".to_string()),
                final_error,
            );

            assert!(warning.message.contains("previous-model marker"));
            assert!(warning.message.contains(&final_message));
            assert_eq!(final_error.to_codex_protocol_error(), expected_info);
            assert!(!matches!(final_error, CodexErr::Fatal(_)));
        }
    }

    #[tokio::test]
    async fn remote_compaction_v2_retry_records_distinct_trace_attempts() -> anyhow::Result<()> {
        crate::test_harness::require_network!();

        let server = responses::start_mock_server().await;
        let request_log = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse_failed(
                    "resp-compact-failed",
                    "server_error",
                    "temporary compaction failure",
                ),
                responses::sse(vec![
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "compaction",
                            "encrypted_content": "encrypted replacement context",
                        }
                    }),
                    responses::ev_completed("resp-compact-succeeded"),
                ]),
            ],
        )
        .await;

        let (mut session, mut turn_context) =
            crate::session::tests::make_session_and_context().await;
        let mut config = (*turn_context.config).clone();
        config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
        config.model_provider.supports_websockets = false;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
        let config = Arc::new(config);
        turn_context.provider = create_model_provider(
            config.model_provider.clone(),
            turn_context.auth_manager.clone(),
        );
        turn_context.config = Arc::clone(&config);
        session.services.model_client = ModelClient::new(
            Some(Arc::clone(&session.services.auth_manager)),
            AgentIdentityAuthPolicy::JwtOnly,
            session.thread_id,
            config.model_provider.clone(),
            turn_context.session_source.clone(),
            turn_context.originator.clone(),
            config.model_verbosity,
            /*enable_request_compression*/ false,
            /*include_timing_metrics*/ false,
            /*beta_features_header*/ None,
            /*concurrent_reasoning_summaries_enabled*/ false,
            /*attestation_provider*/ None,
            config.http_client_factory(),
        );

        let trace_dir = TempDir::new()?;
        let thread_id = session.thread_id.to_string();
        let turn_id = turn_context.sub_id.clone();
        let compaction_id = "compaction-retry-test".to_string();
        let writer = Arc::new(TraceWriter::create(
            trace_dir.path(),
            "trace-retry-test".to_string(),
            "rollout-retry-test".to_string(),
            thread_id.clone(),
        )?);
        writer.append(RawTraceEventPayload::ThreadStarted {
            thread_id: thread_id.clone(),
            agent_path: "/root".to_string(),
            metadata_payload: None,
        })?;
        writer.append(RawTraceEventPayload::CodexTurnStarted {
            codex_turn_id: turn_id.clone(),
            thread_id: thread_id.clone(),
        })?;
        let compaction_trace = CompactionTraceContext::enabled(
            Arc::clone(&writer),
            thread_id,
            turn_id,
            compaction_id.clone(),
            turn_context.model_info.slug.clone(),
            turn_context.provider.info().name.clone(),
        );
        let compaction_trace =
            AsyncCompactionTraceContext::new(&session.terminal_tasks, compaction_trace);

        let prompt = Prompt {
            input: vec![
                message("user", "compact this history", /*phase*/ None),
                ResponseItem::CompactionTrigger {},
            ]
            .into(),
            base_instructions: BaseInstructions {
                text: "compact the conversation".to_string(),
            },
            ..Default::default()
        };
        let compaction_metadata = CompactionTurnMetadata::new(
            CompactionTrigger::Manual,
            CompactionReason::UserRequested,
            CompactionImplementation::ResponsesCompactionV2,
            CompactionPhase::StandaloneTurn,
        );
        let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
            session.installation_id.clone(),
            session.current_window_id().await,
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        );
        let mut client_session = session.services.model_client.new_session();

        struct HeldTracePayload {
            entered: std::sync::mpsc::SyncSender<()>,
            release: std::sync::mpsc::Receiver<()>,
            timed_out: Arc<std::sync::atomic::AtomicBool>,
        }
        impl serde::Serialize for HeldTracePayload {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.entered.send(()).expect("signal the real writer lock");
                if self
                    .release
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .is_err()
                {
                    self.timed_out
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                serializer.serialize_unit()
            }
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holding_writer = std::thread::spawn({
            let writer = Arc::clone(&writer);
            let timed_out = Arc::clone(&timed_out);
            move || {
                writer.write_json_payload(
                    codex_rollout_trace::RawPayloadKind::SessionMetadata,
                    &HeldTracePayload {
                        entered: entered_tx,
                        release: release_rx,
                        timed_out,
                    },
                )
            }
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the real trace writer holds its normal serialization lock");
        let cancellation = CancellationToken::new();
        let mut request = Box::pin(run_remote_compaction_request_v2(
            &session,
            &turn_context,
            &mut client_session,
            &prompt,
            &responses_metadata,
            &compaction_trace,
            &cancellation,
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        // Optional tracing must not hold dispatch or the model result behind
        // storage. Persistence remains ordered and owned by the session drain.
        let output = tokio::time::timeout(Duration::from_secs(1), request.as_mut())
            .await
            .expect("compaction must finish while optional trace storage is blocked")?;
        assert!(
            !timed_out.load(std::sync::atomic::Ordering::SeqCst),
            "trace lock contention must not block the async runtime thread"
        );
        assert_eq!(request_log.requests().len(), 2);
        release_tx
            .send(())
            .expect("release writer after independent async progress");
        holding_writer.join().expect("trace writer thread joins")?;
        drop(request);
        drop(compaction_trace);
        session.terminal_tasks.close();
        session.terminal_tasks.wait().await;
        assert!(
            matches!(output.compaction_output, ResponseItem::Compaction { encrypted_content, .. }
            if encrypted_content == "encrypted replacement context")
        );

        assert_eq!(request_log.requests().len(), 2);
        let rollout = replay_bundle(trace_dir.path())?;
        assert_eq!(rollout.compaction_requests.len(), 2);
        let request_ids = rollout
            .compaction_requests
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(request_ids.len(), 2);
        assert!(rollout.compaction_requests.values().all(|request| {
            request.compaction_id == compaction_id
                && matches!(
                    request.execution.status,
                    ExecutionStatus::Failed | ExecutionStatus::Completed
                )
        }));
        assert_eq!(
            rollout
                .compaction_requests
                .values()
                .filter(|request| request.execution.status == ExecutionStatus::Failed)
                .count(),
            1
        );
        assert_eq!(
            rollout
                .compaction_requests
                .values()
                .filter(|request| request.execution.status == ExecutionStatus::Completed)
                .count(),
            1
        );
        let events = std::fs::read_to_string(trace_dir.path().join("trace.jsonl"))?
            .lines()
            .map(serde_json::from_str::<codex_rollout_trace::RawTraceEvent>)
            .collect::<Result<Vec<_>, _>>()?;
        let attempts = events
            .iter()
            .filter_map(|event| match &event.payload {
                RawTraceEventPayload::CompactionRequestStarted {
                    compaction_request_id,
                    ..
                } => Some(("started", compaction_request_id.as_str())),
                RawTraceEventPayload::CompactionRequestFailed {
                    compaction_request_id,
                    ..
                } => Some(("failed", compaction_request_id.as_str())),
                RawTraceEventPayload::CompactionRequestCompleted {
                    compaction_request_id,
                    ..
                } => Some(("completed", compaction_request_id.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 4);
        assert_eq!(
            attempts.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            vec!["started", "failed", "started", "completed"]
        );
        assert_eq!(attempts[0].1, attempts[1].1);
        assert_eq!(attempts[2].1, attempts[3].1);
        assert_ne!(attempts[0].1, attempts[2].1);
        assert!(
            !events.iter().any(|event| matches!(
                event.payload,
                RawTraceEventPayload::CompactionInstalled { .. }
            )),
            "request attempts alone must not claim history checkpoint installation"
        );

        Ok(())
    }

    #[test]
    fn build_v2_compacted_history_filters_to_installed_retention_shape() {
        let input = vec![
            message("developer", "dev", /*phase*/ None),
            message("system", "sys", /*phase*/ None),
            message("user", "user", /*phase*/ None),
            message("assistant", "commentary", Some(MessagePhase::Commentary)),
            message("assistant", "final", Some(MessagePhase::FinalAnswer)),
            ResponseItem::FunctionCall {
                id: None,
                name: "shell_command".to_string(),
                namespace: None,
                arguments: "{}".to_string(),
                call_id: "call_1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "old".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(input, output.clone());

        assert_eq!(
            history,
            vec![
                message("user", "user", None),
                message("assistant", "final", Some(MessagePhase::FinalAnswer)),
                output
            ]
        );
    }

    #[test]
    fn build_v2_compacted_history_retains_task_and_handoff_before_unresolved_tail() {
        let huge_contextual_message = format!(
            "<environment_context>\n{}\n</environment_context>",
            "c".repeat(20_000)
        );
        let input = vec![
            message("user", "old", /*phase*/ None),
            message(
                "assistant",
                "consumed old request",
                Some(MessagePhase::FinalAnswer),
            ),
            message("user", &huge_contextual_message, /*phase*/ None),
            message("user", "new", /*phase*/ None),
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(input, output.clone());

        assert_eq!(
            history,
            vec![
                message("user", "old", None),
                message(
                    "assistant",
                    "consumed old request",
                    Some(MessagePhase::FinalAnswer)
                ),
                // Untrusted user text is not a runtime context fragment just
                // because it contains an environment_context wrapper.
                message("user", &huge_contextual_message, None),
                message("user", "new", None),
                output,
            ]
        );
    }

    #[tokio::test]
    async fn installed_v2_checkpoint_preserves_unresolved_input_through_provider_filter() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let mut pending = message("user", "Keep the pending user constraint exactly.", None);
        if let ResponseItem::Message { content, .. } = &mut pending {
            content.push(ContentItem::InputImage {
                image_url: "data:image/png;base64,abc".to_string(),
                detail: None,
            });
        }
        let agent = ResponseItem::AgentMessage {
            id: None,
            author: "worker".to_string(),
            recipient: "root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "pending evidence".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        };
        let input = vec![
            message("assistant", "consumed", Some(MessagePhase::FinalAnswer)),
            pending.clone(),
            agent.clone(),
        ];
        session
            .record_conversation_items(&turn, &input)
            .await
            .unwrap();
        let opaque = ResponseItem::Compaction {
            id: None,
            encrypted_content: "checkpoint".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let (retained, image_count) = prepare_v2_retained_input(&session, &input).await.unwrap();
        assert_eq!(image_count, 1);
        let (replacement, baseline, digests) = process_compacted_history_with_retained_input(
            &session,
            &turn,
            vec![
                message("user", "provider transcript to discard", None),
                opaque.clone(),
            ],
            retained,
            &InitialContextInjection::DoNotInject,
        )
        .await
        .unwrap();
        assert_eq!(
            replacement,
            vec![
                message("assistant", "consumed", Some(MessagePhase::FinalAnswer)),
                pending,
                agent,
                opaque
            ]
        );
        let persisted = persisted_v2_compacted_item(replacement.clone());
        assert_eq!(persisted.replacement_history.as_ref(), Some(&replacement));
        session
            .replace_compacted_history(
                &turn,
                replacement.clone(),
                None,
                baseline,
                digests,
                persisted,
            )
            .await
            .unwrap();
        let mut installed = session.clone_history().await.raw_items().to_vec();
        assert!(installed.iter().all(|item| item.id().is_some()));
        for item in &mut installed {
            item.set_id(None);
        }
        assert_eq!(installed, replacement);
    }

    #[tokio::test]
    async fn installed_v2_checkpoint_preserves_task_constraints_and_current_plan() {
        use codex_protocol::plan_tool::PlanItemArg;
        use codex_protocol::plan_tool::StepStatus;
        use codex_protocol::plan_tool::UpdatePlanArgs;

        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let request = message("user", "Implement all external-call categories.", None);
        let correction = message("user", "Finish the work; do not rerun tests.", None);
        let handoff = message(
            "assistant",
            "Network modeling is still missing.",
            Some(MessagePhase::FinalAnswer),
        );
        let input = vec![request.clone(), correction.clone(), handoff.clone()];
        let plan = UpdatePlanArgs {
            explanation: Some("The full contract is not yet satisfied.".to_string()),
            plan: vec![PlanItemArg {
                step: "Finish network modeling".to_string(),
                status: StepStatus::InProgress,
            }],
        };
        session.services.plan_store.update(plan.clone()).await;
        session
            .record_conversation_items(&turn, &input)
            .await
            .unwrap();
        let opaque = ResponseItem::Compaction {
            id: None,
            encrypted_content: "opaque checkpoint".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let (retained, _) = prepare_v2_retained_input(&session, &input).await.unwrap();
        let (replacement, _, _) = process_compacted_history_with_retained_input(
            &session,
            &turn,
            vec![opaque.clone()],
            retained,
            &InitialContextInjection::DoNotInject,
        )
        .await
        .unwrap();
        assert_eq!(&replacement[..3], &[request, correction, handoff]);
        let ResponseItem::Message { content, .. } = &replacement[3] else {
            panic!("expected the current plan in the model checkpoint");
        };
        let text = crate::compact::content_items_to_text(content).unwrap();
        let (current, lineage) = session
            .services
            .plan_store
            .snapshot_with_lineage()
            .await
            .unwrap();
        assert_eq!(current, plan);
        assert!(
            text.contains(&crate::plan_store::plan_revision_with_lineage(
                Some(&plan),
                &lineage
            ))
        );
        assert!(text.contains("not current instructions or proof of completion"));
        assert_eq!(replacement.last(), Some(&opaque));

        // A second compaction must replace the plan fragment rather than accumulate it.
        let mut updated = plan;
        updated.plan[0].step = "Validate the combined implementation".to_string();
        session.services.plan_store.update(updated.clone()).await;
        let (second, _) = prepare_v2_retained_input(&session, &replacement)
            .await
            .unwrap();
        let plans = second
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => {
                    crate::compact::content_items_to_text(content)
                }
                _ => None,
            })
            .filter(|text| text.contains("source=\"compaction_plan\""))
            .collect::<Vec<_>>();
        assert_eq!(plans.len(), 1);
        let (current, lineage) = session
            .services
            .plan_store
            .snapshot_with_lineage()
            .await
            .unwrap();
        assert_eq!(current, updated);
        assert!(
            plans[0].contains(&crate::plan_store::plan_revision_with_lineage(
                Some(&updated),
                &lineage
            ))
        );
        assert!(!plans[0].contains("Finish network modeling"));
    }

    #[tokio::test]
    async fn v2_recovery_failure_keeps_original_history() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let input = vec![message("user", &"exact constraint ".repeat(32_000), None)];
        session
            .record_conversation_items(&turn, &input)
            .await
            .unwrap();
        let original = session.clone_history().await.raw_items().to_vec();
        std::fs::create_dir_all(&turn.config.codex_home).unwrap();
        std::fs::write(turn.config.codex_home.join("tool-output"), "blocked").unwrap();
        let result = prepare_v2_retained_input(&session, &input).await;
        assert!(
            matches!(result, Err(CodexErr::Fatal(message)) if message.contains("could not preserve exact unresolved text"))
        );
        assert_eq!(
            session.clone_history().await.raw_items(),
            original.as_slice()
        );
    }

    #[test]
    fn build_v2_compacted_history_bounds_unresolved_user_text() {
        let text = format!(
            "HEAD_USER_CONSTRAINT\n{}MIDDLE_USER_CONSTRAINT\n{}TAIL_USER_CONSTRAINT",
            "retained ".repeat(10_000),
            "retained ".repeat(10_000),
        );
        let original_tokens = approx_token_count(&text);
        assert!(original_tokens > 16_000);
        let input = vec![ResponseItem::Message {
            id: Some(codex_protocol::ResponseItemId::from_server(
                "unresolved-user-7".to_string(),
            )),
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: text.clone() }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    turn_id: Some("turn-7".to_string()),
                },
            ),
        }];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, retained_image_count) = build_v2_compacted_history(input, output.clone());

        // The bounded user tail and its omission receipt precede the exact opaque checkpoint.
        assert_eq!(history.len(), 3);
        assert_eq!(retained_image_count, 0);
        let ResponseItem::Message { role, content, .. } = &history[0] else {
            panic!("expected bounded unresolved user message");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text: retained }] = content.as_slice() else {
            panic!("expected one retained user text item");
        };
        let retained_tokens = approx_token_count(retained);
        // Literal contract expectations reject both an unbounded tail and the retired 4k cap.
        assert!(retained_tokens > 4_000);
        assert!(retained_tokens <= 16_000);
        assert!(retained.len() < text.len());
        assert!(retained.starts_with("HEAD_USER_CONSTRAINT\n"));
        assert!(retained.contains("MIDDLE_USER_CONSTRAINT\n"));
        assert!(retained.ends_with("TAIL_USER_CONSTRAINT"));
        assert_eq!(history[0].turn_id(), Some("turn-7"));

        let ResponseItem::Message { role, content, .. } = &history[1] else {
            panic!("expected intermediate omission receipt");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text: receipt }] = content.as_slice() else {
            panic!("expected one omission receipt text item");
        };
        let receipt: serde_json::Value =
            serde_json::from_str(receipt).expect("typed text-omission receipt");
        assert_eq!(
            receipt,
            serde_json::json!({
                "version": 1,
                "kind": "codex_local_compaction_text_omission",
                "role": "user",
                "source_item_id": "unresolved-user-7",
                "source_index": 0,
                "recovery_selector": {"kind": "json_pointer", "pointer": "/items/0"},
                "turn_id": "turn-7",
                "original_tokens": original_tokens,
                "retained_tokens": retained_tokens,
                "omitted_tokens": original_tokens - retained_tokens,
                "unresolved": true,
                // Bound to the recovery artifact once one is persisted; this helper builds none.
                "artifact_id": null,
            })
        );
        assert_eq!(history[2], output);
    }

    #[test]
    fn build_v2_compacted_history_retains_unresolved_input_images_within_limits() {
        let input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "user".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,def".to_string(),
                    detail: None,
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, retained_image_count) =
            build_v2_compacted_history(input.clone(), output.clone());

        let mut expected = input;
        expected.push(output);
        assert_eq!(history, expected);
        assert_eq!(retained_image_count, 2);
    }

    #[tokio::test]
    async fn collect_compaction_output_stops_when_owner_is_cancelled() {
        for cancel_before_poll in [false, true] {
            let (_tx_event, rx_event) =
                codex_api::response_event_channel::response_event_channel(1);
            let stream = ResponseStream {
                rx_event,
                attempt_identity: None,
                consumer_dropped: CancellationToken::new(),
            };
            let cancellation_token = CancellationToken::new();
            if cancel_before_poll {
                cancellation_token.cancel();
            }
            let mut pending = Box::pin(collect_compaction_output(
                stream,
                None,
                None,
                &cancellation_token,
            ));
            if !cancel_before_poll {
                assert!(futures::poll!(pending.as_mut()).is_pending());
                cancellation_token.cancel();
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .expect("cancelled compaction collector must stop");
            assert!(matches!(result, Err(CodexErr::TurnAborted)));
        }
    }

    #[tokio::test]
    async fn rejected_checkpoints_and_incomplete_responses_retain_usage_once() {
        for checkpoint in [Some(""), Some("opaque"), None] {
            let (session, turn) = crate::session::tests::make_session_and_context().await;
            let usage = TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                reasoning_output_tokens: 10,
                total_tokens: 120,
                ..Default::default()
            };
            let history = session.clone_history().await;
            let window = session.current_window_id().await;
            let events = match checkpoint {
                Some(content) => vec![
                    Ok(ResponseEvent::OutputItemDone(ResponseItem::Compaction {
                        id: None,
                        encrypted_content: content.to_string(),
                        internal_chat_message_metadata_passthrough: None,
                    })),
                    Ok(ResponseEvent::Completed {
                        response_id: "compacted".into(),
                        token_usage: Some(usage.clone()),
                        end_turn: None,
                    }),
                ],
                None => vec![Err(CodexErr::IncompleteResponse(Box::new(
                    codex_protocol::error::IncompleteResponse {
                        response_id: Some("incomplete".into()),
                        reason: "max_output_tokens".into(),
                        token_usage: Some(usage.clone()),
                    },
                )))],
            };
            let mut receipt = None;
            let result = collect_compaction_output_with_receipt(
                response_stream(events),
                Some(&turn.turn_timing_state),
                Some((&session, &turn)),
                &CancellationToken::new(),
                &mut receipt,
            )
            .await;
            let receipt = receipt.expect("capture the terminal before checkpoint validation");
            assert_eq!(receipt.token_usage.as_ref(), Some(&usage));
            assert_eq!(receipt.provider_completed, checkpoint.is_some());
            assert_eq!(
                receipt.response_id.as_deref(),
                Some(if checkpoint.is_some() {
                    "compacted"
                } else {
                    "incomplete"
                })
            );
            match checkpoint {
                Some("") => assert!(matches!(result, Err(CodexErr::Fatal(message))
                    if message.contains("empty checkpoint"))),
                Some(content) => assert!(matches!(result.unwrap().compaction_output,
                    ResponseItem::Compaction { encrypted_content, .. } if encrypted_content == content)),
                None => assert!(matches!(result, Err(CodexErr::IncompleteResponse(_)))),
            }
            assert_eq!(session.total_token_usage().await, Some(usage));
            assert_eq!(session.current_window_id().await, window);
            assert_eq!(
                session.clone_history().await.raw_items(),
                history.raw_items()
            );
        }
    }

    #[tokio::test]
    async fn failed_compaction_envelope_is_accounted_without_a_checkpoint() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let history = session.clone_history().await;
        let window = session.current_window_id().await;
        let usage = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..Default::default()
        };
        let result = collect_compaction_output(
            response_stream(vec![Err(CodexErr::TerminalResponse {
                source: Box::new(CodexErr::QuotaExceeded),
                response_id: Some("failed".into()),
                token_usage: Some(usage.clone()),
            })]),
            Some(&turn.turn_timing_state),
            Some((&session, &turn)),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(CodexErr::QuotaExceeded)));
        assert_eq!(session.total_token_usage().await, Some(usage));
        assert_eq!(session.current_window_id().await, window);
        assert_eq!(
            session.clone_history().await.raw_items(),
            history.raw_items()
        );
    }

    #[tokio::test]
    async fn collect_compaction_output_does_not_invent_usage_or_completion() {
        for completed in [false, true] {
            let mut events = vec![Ok(ResponseEvent::OutputItemDone(
                ResponseItem::Compaction {
                    id: None,
                    encrypted_content: "encrypted".to_string(),
                    internal_chat_message_metadata_passthrough: None,
                },
            ))];
            if completed {
                events.push(Ok(ResponseEvent::Completed {
                    response_id: "resp-compact".to_string(),
                    token_usage: None,
                    end_turn: Some(true),
                }));
            }
            let timing = Arc::new(TurnTimingState::default());
            timing.mark_turn_started();
            timing.begin_compaction_generation();
            drop(timing.begin_model_request_wait());

            let result = collect_compaction_output(
                response_stream(events),
                Some(&timing),
                None,
                &CancellationToken::new(),
            )
            .await;

            if completed {
                assert!(result.expect("completed compaction").token_usage.is_none());
            } else {
                assert!(matches!(result, Err(CodexErr::Stream(..))));
            }
            let profile = timing.complete_snapshot().protocol_timing();
            assert_eq!(profile.model_requests.len(), 1);
            let request = &profile.model_requests[0];
            assert!(request.token_usage.is_none());
            assert_eq!(request.completed_ms.is_some(), completed);
            assert!(request.first_model_output_ms.is_some());
        }
    }

    #[tokio::test]
    async fn collect_compaction_output_accepts_additional_output_items() {
        let compaction = ResponseItem::Compaction {
            id: None,
            encrypted_content: "encrypted".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let stream = response_stream(vec![
            Ok(ResponseEvent::OutputItemDone(message(
                "assistant",
                "IGNORED_COMPACT_REPLY",
                Some(MessagePhase::FinalAnswer),
            ))),
            Ok(ResponseEvent::OutputItemDone(compaction.clone())),
            Ok(ResponseEvent::Completed {
                response_id: "resp-compact".to_string(),
                token_usage: Some(TokenUsage {
                    input_tokens: 123_456,
                    cached_input_tokens: 7_890,
                    output_tokens: 42,
                    reasoning_output_tokens: 5,
                    total_tokens: 123_498,
                    coverage: None,
                }),
                end_turn: Some(true),
            }),
        ]);

        let timing = Arc::new(TurnTimingState::default());
        timing.mark_turn_started();
        timing.begin_compaction_generation();
        drop(timing.begin_model_request_wait());
        let output =
            collect_compaction_output(stream, Some(&timing), None, &CancellationToken::new())
                .await
                .expect("compaction should be collected");

        assert_eq!(output.compaction_output, compaction);
        assert_eq!(
            output.token_usage,
            Some(TokenUsage {
                input_tokens: 123_456,
                cached_input_tokens: 7_890,
                output_tokens: 42,
                reasoning_output_tokens: 5,
                total_tokens: 123_498,
                coverage: None,
            })
        );
        let profile = timing.complete_snapshot().protocol_timing();
        assert_eq!(profile.model_requests.len(), 1);
        let request = &profile.model_requests[0];
        let usage = request.token_usage.as_ref().expect("compaction usage");
        assert_eq!(usage.input_tokens, 123_456);
        assert_eq!(usage.cached_input_tokens, 7_890);
        assert_eq!(usage.visible_output_tokens, 37);
        assert_eq!(usage.reasoning_tokens, 5);
        assert_eq!(usage.total_tokens, 123_498);
        assert_eq!(request.output_tokens, 42);
        assert_eq!(request.reasoning_output_tokens, 5);
        assert!(request.first_model_output_ms.is_some());
        assert!(request.completed_ms.is_some());
        assert_eq!(
            request.first_actionable_output_ms,
            request.first_model_output_ms
        );
        assert_eq!(profile.milestones.first_visible_output_ms, None);
        for count in [0, 2] {
            let mut events = (0..count)
                .map(|_| Ok(ResponseEvent::OutputItemDone(compaction.clone())))
                .collect::<Vec<_>>();
            events.push(Ok(ResponseEvent::Completed {
                response_id: "invalid-count".into(),
                token_usage: None,
                end_turn: Some(true),
            }));
            let result = collect_compaction_output(
                response_stream(events),
                None,
                None,
                &CancellationToken::new(),
            )
            .await;
            assert!(matches!(result, Err(CodexErr::Fatal(message))
                if message.contains(&format!("got {count} from {count} output items"))));
        }
    }

    #[tokio::test]
    async fn audit146x_rejected_remote_checkpoints_allow_model_and_format_fallback() {
        use crate::compact_model_fallback::is_compaction_contract_violation;
        use crate::compact_model_fallback::should_retry_with_current_model;
        use crate::compact_model_fallback::should_retry_with_local_compaction;
        let checkpoint = |content: &str| -> CodexResult<ResponseEvent> {
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Compaction {
                id: None,
                encrypted_content: content.to_string(),
                internal_chat_message_metadata_passthrough: None,
            }))
        };
        let completed = || -> CodexResult<ResponseEvent> {
            Ok(ResponseEvent::Completed {
                response_id: "rejected".into(),
                token_usage: None,
                end_turn: Some(true),
            })
        };
        for events in [
            vec![completed()],
            vec![checkpoint("first"), checkpoint("second"), completed()],
            vec![checkpoint(""), completed()],
        ] {
            let Err(error) = collect_compaction_output(
                response_stream(events),
                None,
                None,
                &CancellationToken::new(),
            )
            .await
            else {
                panic!("rejected checkpoint output must fail");
            };
            assert!(is_compaction_contract_violation(&error), "{error}");
            assert!(should_retry_with_current_model(&error), "{error}");
            assert!(should_retry_with_local_compaction(&error), "{error}");
        }
        // Negative control: a stream that closes before completion keeps its
        // transport classification instead of becoming a rejected checkpoint.
        let Err(error) = collect_compaction_output(
            response_stream(vec![checkpoint("opaque")]),
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        else {
            panic!("an incomplete stream must fail");
        };
        assert!(matches!(error, CodexErr::Stream(..)), "{error}");
        assert!(!is_compaction_contract_violation(&error));
    }
}
#[cfg(test)]
mod ctxaudit_usage_tests {
    use super::*;

    #[tokio::test]
    async fn ctxaudit_rejected_completed_compaction_counts_usage_without_changing_context() {
        for count in [0, 1, 2] {
            let (session, turn) = crate::session::tests::make_session_and_context().await;
            let prior = TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                ..Default::default()
            };
            session
                .update_token_usage_info(&turn, Some(&prior))
                .await
                .unwrap();
            let before = session.token_usage_info().await.unwrap();
            let usage = TokenUsage {
                input_tokens: 25,
                cached_input_tokens: 5,
                output_tokens: 7,
                reasoning_output_tokens: 2,
                total_tokens: 32,
                coverage: None,
            };
            let mut events = (0..count)
                .map(|_| {
                    Ok(ResponseEvent::OutputItemDone(ResponseItem::Compaction {
                        id: None,
                        encrypted_content: "checkpoint".into(),
                        internal_chat_message_metadata_passthrough: None,
                    }))
                })
                .collect::<Vec<_>>();
            events.push(Ok(ResponseEvent::Completed {
                response_id: "audit".into(),
                token_usage: Some(usage.clone()),
                end_turn: Some(true),
            }));
            let (tx, rx) = codex_api::response_event_channel::response_event_channel(events.len());
            for event in events {
                tx.send(event).await.unwrap();
            }
            drop(tx);
            let stream = ResponseStream {
                rx_event: rx,
                attempt_identity: None,
                consumer_dropped: CancellationToken::new(),
            };
            let result = collect_compaction_output(
                stream,
                None,
                Some((&session, &turn)),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(result.is_ok(), count == 1);
            let after = session.token_usage_info().await.unwrap();
            let mut total = before.total_token_usage.clone();
            total.add_assign(&usage);
            assert_eq!(after.total_token_usage, total);
            assert_eq!(after.last_token_usage, before.last_token_usage);
            assert_eq!(
                after.estimated_context_tokens,
                before.estimated_context_tokens
            );
        }
    }
}

#[cfg(test)]
#[path = "compact_remote_v2_audit146_tests.rs"]
mod audit146_tests;

#[cfg(test)]
#[path = "compact_remote_v2_manual_tests.rs"]
mod manual_tests;
