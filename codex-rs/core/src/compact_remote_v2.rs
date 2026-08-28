use std::sync::Arc;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactionAnalyticsAttempt;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact::InitialContextInjection;
use crate::compact::build_unresolved_user_history;
use crate::compact::compaction_status_from_result;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote::process_compacted_history;
use crate::hook_runtime::run_post_compact_hook_gate;
use crate::hook_runtime::run_pre_compact_hook_gate;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::turn_timing::TurnTimingState;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
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
use codex_rollout_trace::CompactionCheckpointTracePayload;
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;

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
    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
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
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        /*fallback_step_context*/ None,
        /*client_session*/ None,
        InitialContextInjection::AtStart(world_state),
        compaction_metadata,
        cancellation_token,
    )
    .await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    fallback_step_context: Option<&Arc<StepContext>>,
    client_session: Option<&mut ModelClientSession>,
    initial_context_injection: InitialContextInjection,
    compaction_metadata: CompactionTurnMetadata,
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
    let result = run_remote_compact_task_inner_impl(
        sess,
        step_context,
        fallback_step_context,
        client_session,
        initial_context_injection,
        compaction_metadata,
        &mut analytics_details,
        cancellation_token,
    )
    .await;
    let status = compaction_status_from_result(&result);
    let codex_error = result.as_ref().err();
    if result.is_ok() {
        let recovery_summary = sess
            .services
            .task_evidence
            .compaction_recovery_summary()
            .await;
        if run_post_compact_hook_gate(sess, turn_context, trigger, Some(&recovery_summary)).await {
            attempt
                .track(sess.as_ref(), status, codex_error, analytics_details)
                .await;
            return Err(CodexErr::TurnAborted);
        }
    }
    attempt
        .track(sess.as_ref(), status, codex_error, analytics_details)
        .await;
    match result {
        Ok(()) => Ok(()),
        Err(err @ CodexErr::TurnAborted) => Err(err),
        Err(err) => {
            sess.track_turn_codex_error(turn_context, &err);
            let event = EventMsg::Error(
                err.to_error_event(Some("Error running remote compact task".to_string())),
            );
            sess.send_event(turn_context, event).await;
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
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let context_compaction_item = ContextCompactionItem::new();
    let compaction_id = context_compaction_item.id.clone();
    let compaction_trace = sess.services.rollout_thread_trace.compaction_trace_context(
        turn_context.sub_id.as_str(),
        compaction_id.as_str(),
        turn_context.model_info.slug.as_str(),
        turn_context.provider.info().name.as_str(),
    );
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item);
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;

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
            let fallback_turn_context = &fallback_step_context.turn;
            let fallback_compaction_trace =
                sess.services.rollout_thread_trace.compaction_trace_context(
                    fallback_turn_context.sub_id.as_str(),
                    compaction_id.as_str(),
                    fallback_turn_context.model_info.slug.as_str(),
                    fallback_turn_context.provider.info().name.as_str(),
                );
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
        prompt_input,
        compaction_output,
        token_usage,
        stable_context_fingerprint,
        owned_client_session: _owned_client_session,
    } = attempt;
    if let Some(token_usage) = token_usage {
        analytics_details.active_context_tokens_before = Some(token_usage.input_tokens);
        analytics_details.compaction_summary_tokens = Some(token_usage.output_tokens);
        analytics_details.cached_input_tokens = Some(token_usage.cached_input_tokens);
    }
    let (compacted_history, retained_images) =
        build_v2_compacted_history(prompt_input, compaction_output);
    analytics_details.retained_image_count = Some(retained_images);
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (new_history, world_state_baseline, fragment_digests) = process_compacted_history(
        sess.as_ref(),
        compaction_turn_context.as_ref(),
        compacted_history,
        &initial_context_injection,
    )
    .await;

    let reference_context_item = match &initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::AtStart(_) => Some(compaction_turn_context.to_turn_context_item()),
        #[cfg(test)]
        InitialContextInjection::BeforeLastUserMessage(_) => {
            Some(compaction_turn_context.to_turn_context_item())
        }
    };
    let compacted_request_prefix = new_history
        .split_last()
        .map_or_else(Vec::new, |(_, prefix)| prefix.to_vec());
    let compacted_item = persisted_v2_compacted_item(
        new_history.clone(),
        new_window_number,
        new_window_ids.first_window_id.to_string(),
        new_window_ids.previous_window_id.map(|id| id.to_string()),
        new_window_ids.window_id.to_string(),
    );
    if let Some(trace_input_history) = trace_input_history.as_deref() {
        compaction_trace.record_installed(&CompactionCheckpointTracePayload {
            input_history: trace_input_history,
            replacement_history: &new_history,
        });
    }
    sess.replace_compacted_history(
        compaction_turn_context.as_ref(),
        new_history,
        reference_context_item,
        world_state_baseline,
        fragment_digests,
        compacted_item,
    )
    .await;
    if let Some(client_session) = client_session {
        client_session.rebase_remote_compaction_history(
            &compacted_request_prefix,
            stable_context_fingerprint,
        );
    }
    sess.recompute_token_usage(compaction_turn_context).await;

    sess.emit_turn_item_completed(compaction_turn_context, compaction_item)
        .await;
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
    compaction_trace: &CompactionTraceContext,
    cancellation_token: &CancellationToken,
) -> CodexResult<RemoteCompactionV2Output> {
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retries = 0;
    turn_context.turn_timing_state.begin_compaction_generation();
    loop {
        let trace_attempt = compaction_trace.start_attempt(&RemoteCompactionV2TraceRequest {
            model: turn_context.model_info.slug.as_str(),
            instructions: prompt.base_instructions.text.as_str(),
            input: &prompt.input,
            parallel_tool_calls: prompt.parallel_tool_calls,
        });
        let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
        let inference_trace_context = InferenceTraceContext::disabled();
        let stream_result = tokio::select! {
            _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
            result = client_session.stream(
                prompt,
                &turn_context.model_info,
                &turn_context.session_telemetry,
                crate::client::request_effort_for_model(
                    &turn_context.model_info,
                    turn_context.reasoning_effort.clone(),
                ),
                turn_context.reasoning_summary,
                turn_context.config.service_tier.clone(),
                responses_metadata,
                &inference_trace_context,
            ) => result,
        };
        drop(model_request_timing_guard);
        let result = match stream_result {
            Ok(stream) => {
                collect_compaction_output(
                    stream,
                    Some(&turn_context.turn_timing_state),
                    cancellation_token,
                )
                .await
            }
            Err(err) => Err(err),
        };
        trace_attempt.record_result(
            result
                .as_ref()
                .map(|output| std::slice::from_ref(&output.compaction_output)),
        );

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) if !err.is_retryable() => return Err(err),
            Err(err) => {
                handle_retryable_response_stream_error(
                    &mut retries,
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
        let _model_stream_processing_timing_guard =
            timing_state.map(super::turn_timing::TurnTimingState::begin_model_stream_processing);
        match event? {
            ResponseEvent::OutputItemDone(item) => {
                output_item_count += 1;
                if let ResponseItem::Compaction { .. } = item {
                    compaction_count += 1;
                    if compaction_output.is_none() {
                        compaction_output = Some(item);
                    }
                }
            }
            ResponseEvent::Completed { token_usage, .. } => {
                saw_completed = true;
                completed_token_usage = token_usage;
                break;
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
    Ok(RemoteCompactionV2Output {
        compaction_output,
        token_usage: completed_token_usage,
    })
}

fn build_v2_compacted_history(
    prompt_input: Vec<ResponseItem>,
    compaction_output: ResponseItem,
) -> (Vec<ResponseItem>, usize) {
    // The opaque compaction item owns the consumed transcript. Preserve only the bounded exact
    // user tail after the last model-generated boundary; current instructions and durable task
    // state are re-injected by `process_compacted_history`.
    let (mut history, retained_image_count) = build_unresolved_user_history(&prompt_input);
    history.push(compaction_output);
    (history, retained_image_count)
}

fn persisted_v2_compacted_item(
    replacement_history: Vec<ResponseItem>,
    window_number: u64,
    first_window_id: String,
    previous_window_id: Option<String>,
    window_id: String,
) -> CompactedItem {
    CompactedItem {
        message: String::new(),
        // Resume and fork reconstruction must install the same opaque checkpoint and bounded
        // unresolved user tail that became live above. `None` is reserved for legacy records.
        replacement_history: Some(replacement_history),
        window_number: Some(window_number),
        first_window_id: Some(first_window_id),
        previous_window_id,
        window_id: Some(window_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelClient;
    use crate::compact::content_items_to_text;
    use crate::responses_metadata::CodexResponsesRequestKind;
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
    use core_test_support::responses;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
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
        let (tx_event, rx_event) = mpsc::channel(events.len().max(1));
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

    #[test]
    fn dual_model_failure_preserves_final_typed_error_and_both_diagnostics() {
        use codex_protocol::error::UsageLimitReachedError;
        use codex_protocol::protocol::CodexErrorInfo;

        let cases = [
            (CodexErr::ServerOverloaded, CodexErrorInfo::ServerOverloaded),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_compaction_v2_retry_records_distinct_trace_attempts() -> anyhow::Result<()> {
        core_test_support::skip_if_no_network!(Ok(()));

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
            writer,
            thread_id,
            turn_id,
            compaction_id.clone(),
            turn_context.model_info.slug.clone(),
            turn_context.provider.info().name.clone(),
        );

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

        let output = run_remote_compaction_request_v2(
            &session,
            &turn_context,
            &mut client_session,
            &prompt,
            &responses_metadata,
            &compaction_trace,
            &CancellationToken::new(),
        )
        .await?;
        let input_history = vec![prompt.input[0].clone()];
        let replacement_history = vec![output.compaction_output];
        compaction_trace.record_installed(&CompactionCheckpointTracePayload {
            input_history: &input_history,
            replacement_history: &replacement_history,
        });

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
        let installed = rollout
            .compactions
            .get(&compaction_id)
            .expect("successful retry should install one compaction checkpoint");
        assert_eq!(rollout.compactions.len(), 1);
        assert_eq!(
            installed
                .request_ids
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            request_ids
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

        assert_eq!(history, vec![output]);
    }

    #[test]
    fn build_v2_compacted_history_retains_only_unresolved_user_tail() {
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

        assert_eq!(history, vec![message("user", "new", None), output]);
    }

    #[test]
    fn build_v2_compacted_history_retains_unresolved_agent_input() {
        let agent_message = ResponseItem::AgentMessage {
            id: None,
            author: "worker".to_string(),
            recipient: "root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "unconsumed worker evidence".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        };
        let input = vec![
            message("assistant", "consumed", Some(MessagePhase::FinalAnswer)),
            agent_message.clone(),
        ];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(input, output.clone());

        assert_eq!(history, vec![agent_message, output]);
    }

    #[test]
    fn build_v2_compacted_history_bounds_unresolved_user_text() {
        let input = vec![message(
            "user",
            &"retained ".repeat(20_000),
            /*phase*/ None,
        )];
        let output = ResponseItem::Compaction {
            id: None,
            encrypted_content: "new".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };

        let (history, _) = build_v2_compacted_history(input, output.clone());

        assert_eq!(history.len(), 2);
        let ResponseItem::Message { content, .. } = &history[0] else {
            panic!("expected bounded unresolved user message");
        };
        let retained = content_items_to_text(content).expect("retained user text");
        assert!(approx_token_count(&retained) <= 4_000);
        assert_eq!(history[1], output);
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

        let (history, retained_image_count) = build_v2_compacted_history(input, output.clone());

        assert_eq!(history.len(), 2);
        let ResponseItem::Message { content, .. } = &history[0] else {
            panic!("expected unresolved image message");
        };
        assert_eq!(
            content
                .iter()
                .filter(|item| matches!(item, ContentItem::InputImage { .. }))
                .count(),
            2
        );
        assert_eq!(history[1], output);
        assert_eq!(retained_image_count, 2);
    }

    #[test]
    fn persisted_v2_compacted_item_carries_exact_replacement_history() {
        let replacement_history = vec![
            message("user", "unresolved", None),
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "opaque".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        ];

        let persisted = persisted_v2_compacted_item(
            replacement_history.clone(),
            2,
            "first".to_string(),
            Some("previous".to_string()),
            "current".to_string(),
        );

        assert_eq!(persisted.replacement_history, Some(replacement_history));
    }

    #[tokio::test]
    async fn collect_compaction_output_stops_when_owner_is_cancelled() {
        let (_tx_event, rx_event) = mpsc::channel(1);
        let stream = ResponseStream {
            rx_event,
            attempt_identity: None,
            consumer_dropped: CancellationToken::new(),
        };
        let cancellation_token = CancellationToken::new();
        cancellation_token.cancel();

        let result = collect_compaction_output(stream, None, &cancellation_token).await;

        assert!(matches!(result, Err(CodexErr::TurnAborted)));
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
                }),
                end_turn: Some(true),
            }),
        ]);

        let output = collect_compaction_output(stream, None, &CancellationToken::new())
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
            })
        );
    }
}
