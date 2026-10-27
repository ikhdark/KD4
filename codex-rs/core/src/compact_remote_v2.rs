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
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;


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
    crate::responses_retry::wait_for_response_retry_after(
        &error,
        sess.as_ref(),
        turn_context.as_ref(),
        cancellation_token,
    )
    .await?;
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

    // Each attempt discounts its own trimming from this pre-compaction baseline.
    let active_context_tokens_before = analytics_details.active_context_tokens_before;
    let attempt = run_remote_compact_v2_attempt(
        sess,
        step_context,
        client_session.as_deref_mut(),
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
            crate::responses_retry::wait_for_response_retry_after(
                &error,
                sess.as_ref(),
                turn_context.as_ref(),
                cancellation_token,
            )
            .await?;
            let fallback_turn_context = &fallback_step_context.turn;
            analytics_details.active_context_tokens_before = active_context_tokens_before;
            let fallback_result = run_remote_compact_v2_attempt(
                sess,
                fallback_step_context,
                client_session.as_deref_mut(),
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
        InitialContextInjection::AtStart(_) => {
            Some(compaction_turn_context.to_turn_context_item_async().await)
        }
    };
    let compacted_item = persisted_v2_compacted_item(new_history.clone());
    sess.replace_compacted_history(
        compaction_turn_context,
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
        compacted_item,
    )
    .await?;
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


async fn run_remote_compaction_request_v2(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
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
        retry_state.check_retry_budget(cancellation_token)?;
        let attempt_started = tokio::time::Instant::now();
        let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
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
            ) => result,
        };
        drop(model_request_timing_guard);
        sess.bind_rate_limit_account(client_session.dispatched_account().cloned())
            .await;
        if let Err(CodexErr::UsageLimitReached(error)) = &stream_result
            && let Some(snapshot) = error.rate_limits.as_deref()
        {
            sess.update_rate_limits(turn_context, snapshot.clone()).await;
        }
        let result = match stream_result {
            Ok(stream) => {
                collect_compaction_output(
                    stream,
                    Some(&turn_context.turn_timing_state),
                    Some((sess, turn_context)),
                    cancellation_token,
                )
                .await
            }
            Err(err) => Err(err),
        };

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


async fn collect_compaction_output(
    mut stream: ResponseStream,
    timing_state: Option<&Arc<TurnTimingState>>,
    accounting_context: Option<(&Session, &TurnContext)>,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactionV2Output> {
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
                let error = error.into_response_error();
                if let (Some((sess, turn)), CodexErr::UsageLimitReached(limit)) =
                    (accounting_context, &error)
                    && let Some(snapshot) = limit.rate_limits.as_deref()
                {
                    sess.update_rate_limits(turn, snapshot.clone()).await;
                }
                return Err(error);
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
                token_usage,
                ..
            } => {
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

async fn prepare_v2_retained_input_source(
    sess: &Session,
    prompt_input: &[ResponseItem],
) -> CodexResult<crate::compact::TaskInputCheckpoint> {
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

