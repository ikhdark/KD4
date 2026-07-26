use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::apply_skill_injection_observability;
use crate::client::ModelClientSession;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::collect_explicit_skill_mentions;
use crate::compact::InitialContextInjection;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::compact_remote_v2::run_inline_remote_auto_compact_task as run_inline_remote_auto_compact_task_v2;
use crate::connectors;
use crate::context::ContextualUserFragment;
use crate::feedback_tags;
use crate::hook_runtime::inspect_pending_input;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::record_pending_input;
use crate::hook_runtime::run_legacy_after_agent_hook;
use crate::hook_runtime::run_pending_session_start_hooks;
use crate::hook_runtime::run_turn_stop_hooks;
use crate::injection::ToolMentionKind;
use crate::injection::app_id_from_path;
use crate::injection::tool_kind_for_path;
use crate::mcp_skill_dependencies::McpDependencyEffectOutcome;
use crate::mcp_skill_dependencies::PlannedMcpDependencyEffect;
use crate::mcp_skill_dependencies::apply_mcp_dependency_effect;
use crate::mcp_skill_dependencies::inventory_contains_expected;
use crate::mcp_skill_dependencies::plan_mcp_dependencies;
use crate::mcp_tool_exposure::build_mcp_tool_exposure;
use crate::mentions::build_connector_slug_counts;
use crate::mentions::build_skill_name_counts;
use crate::mentions::collect_explicit_app_ids;
use crate::mentions::collect_explicit_plugin_mentions;
use crate::mentions::collect_tool_mentions_from_messages;
use crate::pending_turn_plan::CompletedEffect;
use crate::pending_turn_plan::EffectImpact;
use crate::pending_turn_plan::FixedPointPlanningState;
use crate::pending_turn_plan::PlanningSnapshotIdentity;
use crate::plan_skill_injections;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::build_plugin_injections;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::FinalCommitBoundary;
use crate::session::PreviousTurnSettings;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::stream_events_utils::FinalizedTurnItem;
use crate::stream_events_utils::FinalizedTurnItemFacts;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use crate::stream_events_utils::raw_assistant_output_text_from_item;
use crate::stream_events_utils::record_completed_response_item_with_finalized_facts;
use crate::task_evidence::ManagedFinalState;
use crate::task_evidence::TaskContractUpdate;
use crate::task_evidence::TaskLifecycleStatus;
use crate::task_evidence::TaskPhase;
use crate::tasks::emit_compact_metric;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolRouterParams;
use crate::tools::router::ToolSuggestCandidates;
use crate::tools::router::ToolSuggestPresentation;
use crate::tools::router::extension_tool_executors;
use crate::tools::spec_plan::search_tool_enabled;
use crate::tools::spec_plan::tool_suggest_enabled;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::TurnLocalPhase;
use crate::turn_timing::TurnTimingGuard;
use crate::turn_timing::record_turn_ttft_metric;
use crate::util::error_or_panic;
use codex_analytics::AppInvocation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::InvocationType;
use codex_analytics::TrackEventsContext;
use codex_analytics::TurnResolvedConfigFact;
use codex_analytics::build_track_events_context;
use codex_async_utils::OrCancelExt;
use codex_core_plugins::RecommendedPluginCandidatesInput;
use codex_core_skills::injection::InjectedHostSkillPrompts;
use codex_core_skills::injection::PlannedSkillInjections;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputEnvironment;
use codex_features::Feature;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::PlanItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::build_hook_prompt_message;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentReasoningSectionBreakEvent;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::PlanDeltaEvent;
use codex_protocol::protocol::ReasoningContentDeltaEvent;
use codex_protocol::protocol::ReasoningRawContentDeltaEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SafetyBufferingEvent;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnDiffEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_tools::ToolName;
use codex_tools::filter_request_plugin_install_discoverable_tools_for_client;
use codex_utils_stream_parser::AssistantTextChunk;
use codex_utils_stream_parser::AssistantTextStreamParser;
use codex_utils_stream_parser::ProposedPlanSegment;
use codex_utils_stream_parser::extract_proposed_plan_text;
use codex_utils_stream_parser::strip_citations;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use sha2::Digest;
use sha2::Sha256;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

const POST_SAMPLING_TOKEN_ESTIMATE_TARGET: &str = "codex_core::post_sampling_token_estimate";

/// Takes initial turn input and runs a loop where, at each sampling request,
/// the model replies with either:
///
/// - requested function calls
/// - an assistant message
///
/// While it is possible for the model to return multiple of these items in a
/// single sampling request, in practice, we generally one item per sampling request:
///
/// - If the model requests a function call, we execute it and send the output
///   back to the model in the next sampling request.
/// - If the model sends only an assistant message, we record it in the
///   conversation history and consider the turn complete.
///
pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    input: Vec<TurnInput>,
    prewarmed_client_session: Option<ModelClientSession>,
    cancellation_token: CancellationToken,
) -> CodexResult<Option<String>> {
    super::rollout_budget::maybe_approve_additional_tranche(sess.as_ref(), &input);
    let mut preparation_timing_guard = Some(
        turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Preparation),
    );
    let mut client_session =
        prewarmed_client_session.unwrap_or_else(|| sess.services.model_client.new_session());
    let planning_timing_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::Planning);
    let pending_turn_plan_result = stabilize_pending_turn_plan(
        &sess,
        &turn_context,
        &input,
        &mut client_session,
        &cancellation_token,
    )
    .await;
    drop(planning_timing_guard);
    let pending_turn_plan = match pending_turn_plan_result {
        Ok(plan) => plan,
        Err(err) => {
            if matches!(err, CodexErr::TurnAborted) {
                run_hooks_and_record_inputs(&sess, &turn_context, &input).await;
                return Err(err);
            }
            let error = err.to_codex_protocol_error();
            sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                .await;
            error!("Pending-turn planning failed before persistence or model send: {err}");
            return Ok(None);
        }
    };
    let PendingTurnPlan {
        step_context: first_step_context,
        first_router,
        injection_items,
        explicitly_enabled_connectors,
        pre_sampling_context_limit_compaction_completed,
        ..
    } = pending_turn_plan;

    // Pending-turn planning is now stable and all required effects have completed.
    // Only now may normal turn persistence begin.
    let (mut world_state, display_roots) = tokio::join!(
        sess.record_context_updates_and_set_reference_context_item(first_step_context.as_ref()),
        turn_diff_display_roots(sess.as_ref(), turn_context.as_ref()),
    );

    if run_pending_session_start_hooks(&sess, &turn_context).await {
        return Ok(None);
    }
    let mut can_drain_pending_input = input.is_empty();
    if run_hooks_and_record_inputs(&sess, &turn_context, &input).await {
        return Ok(None);
    }

    sess.merge_connector_selection(explicitly_enabled_connectors.clone())
        .await;
    sess.set_previous_turn_settings(Some(PreviousTurnSettings {
        model: turn_context.model_info.slug.clone(),
        comp_hash: turn_context.model_info.comp_hash.clone(),
        realtime_active: Some(turn_context.realtime_active),
    }))
    .await;
    if !injection_items.is_empty() {
        if crate::latency_switches::stage3_persistence_history_enabled() {
            sess.record_conversation_items(&turn_context, &injection_items)
                .await;
        } else {
            for item in &injection_items {
                sess.record_conversation_items(&turn_context, std::slice::from_ref(item))
                    .await;
            }
        }
    }

    track_turn_resolved_config_analytics(&sess, &turn_context, &input).await;

    let mut last_agent_message: Option<String> = None;
    let mut stop_hook_active = false;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(
        TurnDiffTracker::with_environment_display_roots(display_roots),
    ));

    // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
    // one instance across retries within this turn.
    // Pending input is drained into history before building the next model request.
    // However, we defer that drain until after sampling in two cases:
    // 1. At the start of a turn, so the fresh turn input in `input` gets sampled first.
    // 2. After auto-compact, when model/tool continuation needs to resume before any steer.

    let mut next_step_context = Some(first_step_context);
    let mut first_router = Some(first_router);
    let mut context_limit_compaction_attempted = pre_sampling_context_limit_compaction_completed;
    let mut has_sampled = false;
    let mut model_continuation_pending = false;
    loop {
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        let pending_input = if can_drain_pending_input {
            sess.input_queue.get_pending_input(&sess.active_turn).await
        } else {
            Vec::new()
        };
        let pending_task_contract = task_contract_from_inter_agent_input(&pending_input);
        if !pending_task_contract.is_empty() {
            let task_contract_update = sess
                .services
                .task_evidence
                .extend_task_contract(&turn_context.sub_id, &pending_task_contract)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            if task_contract_update == TaskContractUpdate::FinalCommitted {
                return Err(CodexErr::InvalidRequest(
                    "pending task input arrived after final output committed".to_string(),
                ));
            }
        }

        if run_hooks_and_record_inputs(&sess, &turn_context, &pending_input).await {
            break;
        }

        let window_id = sess.current_window_id().await;
        super::rollout_budget::maybe_record_reminder(
            sess.as_ref(),
            turn_context.as_ref(),
            &window_id,
        )
        .await;

        // Capture once so context, advertised tools, and tool calls share one request view.
        let step_context = match next_step_context.take() {
            Some(step_context) => step_context,
            None => sess.capture_step_context(Arc::clone(&turn_context)).await,
        };
        let sampling_request_result: CodexResult<_> = async {
            super::time_reminder::maybe_record_current_time_reminder(
                sess.as_ref(),
                turn_context.as_ref(),
                &window_id,
            )
            .await?;

            if turn_context
                .config
                .features
                .enabled(Feature::DeferredExecutor)
            {
                world_state = sess
                    .record_step_world_state_if_changed(&world_state, step_context.as_ref())
                    .await;
            }

            // Construct the input that we will send to the model.
            let sampling_request_input: Vec<ResponseItem> = async {
                sess.clone_history()
                    .await
                    .for_prompt(&turn_context.model_info.input_modalities)
            }
            .instrument(trace_span!("run_turn.prepare_sampling_request_input"))
            .await;

            let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
                sess.installation_id.clone(),
                window_id,
                CodexResponsesRequestKind::Turn,
            );
            run_sampling_request(
                Arc::clone(&sess),
                Arc::clone(&step_context),
                Arc::clone(&turn_extension_data),
                Arc::clone(&turn_diff_tracker),
                &mut client_session,
                &responses_metadata,
                sampling_request_input,
                &mut first_router,
                &mut preparation_timing_guard,
                cancellation_token.child_token(),
            )
            .await
        }
        .await;
        match sampling_request_result {
            Ok(RunSamplingRequestOutcome::Sampled(
                sampling_request_output,
                sampling_request_input,
            )) => {
                has_sampled = true;
                context_limit_compaction_attempted = false;
                let SamplingRequestResult {
                    needs_follow_up: model_needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                    mut pending_managed_final,
                    mut models_refresh_task,
                } = sampling_request_output;
                let mut managed_final_committed = false;
                model_continuation_pending = model_needs_follow_up;
                can_drain_pending_input = true;
                let (has_pending_input, token_status) = async {
                    let has_pending_input =
                        sess.input_queue.has_pending_input(&sess.active_turn).await;
                    let token_status = super::context_window::context_window_token_status(
                        sess.as_ref(),
                        turn_context.as_ref(),
                    )
                    .await;
                    (has_pending_input, token_status)
                }
                .instrument(trace_span!("run_turn.collect_post_sampling_state"))
                .await;
                let needs_follow_up = model_needs_follow_up || has_pending_input;
                if needs_follow_up {
                    abort_pending_managed_final_reservation(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &mut pending_managed_final,
                    )
                    .await?;
                }
                if cancellation_token.is_cancelled() {
                    abort_pending_managed_final_reservation(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &mut pending_managed_final,
                    )
                    .await?;
                    return Err(CodexErr::TurnAborted);
                }
                let token_limit_reached = token_status.token_limit_reached;

                trace!(
                    turn_id = %turn_context.sub_id,
                    total_usage_tokens = token_status.active_context_tokens,
                    auto_compact_scope_tokens = token_status.auto_compact_scope_tokens,
                    auto_compact_scope_limit = ?token_status.auto_compact_scope_limit,
                    auto_compact_limit_scope = ?turn_context.config.model_auto_compact_token_limit_scope,
                    auto_compact_window_prefill_tokens = ?token_status.auto_compact_window_prefill_tokens,
                    full_context_window_limit = ?token_status.full_context_window_limit,
                    full_context_window_limit_reached = token_status.full_context_window_limit_reached,
                    token_limit_reached,
                    model_needs_follow_up,
                    has_pending_input,
                    needs_follow_up,
                    "post sampling token usage"
                );
                if tracing::event_enabled!(
                    target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
                    tracing::Level::TRACE,
                    turn_id,
                    estimated_token_count,
                    message
                ) {
                    let estimated_token_count =
                        sess.get_estimated_token_count(turn_context.as_ref()).await;
                    trace!(
                        target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
                        turn_id = %turn_context.sub_id,
                        estimated_token_count = ?estimated_token_count,
                        "post sampling token estimate"
                    );
                }

                super::token_budget::maybe_record(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    token_status.tokens_until_compaction,
                )
                .await;

                // as long as compaction works well in getting us way below the token limit, we shouldn't worry about being in an infinite loop.
                if needs_follow_up
                    && (sess.take_new_context_window_request().await || token_limit_reached)
                {
                    if let Err(err) = run_auto_compact(
                        &sess,
                        Arc::clone(&step_context),
                        /*fallback_step_context*/ None,
                        &mut client_session,
                        InitialContextInjection::BeforeLastUserMessage(Arc::clone(&world_state)),
                        CompactionReason::ContextLimit,
                        CompactionPhase::MidTurn,
                    )
                    .await
                    {
                        if matches!(err, CodexErr::TurnAborted) {
                            return Err(err);
                        }
                        let error = err.to_codex_protocol_error();
                        sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                            .await;
                        return Ok(None);
                    }
                    client_session.invalidate_incremental_history("mid-turn compaction");
                    can_drain_pending_input = !model_needs_follow_up;
                    context_limit_compaction_attempted = true;
                    continue;
                }

                if !needs_follow_up {
                    last_agent_message = sampling_request_last_agent_message;
                    if let Some(changed_paths) = sess
                        .services
                        .task_evidence
                        .take_automatic_verify_plan_request()
                        .await
                        && let Err(err) = crate::tools::handlers::run_automatic_verify_local_plan(
                            Arc::clone(&sess),
                            Arc::clone(&step_context),
                            Arc::clone(&turn_diff_tracker),
                            changed_paths,
                            cancellation_token.child_token(),
                        )
                        .await
                    {
                        sess.send_event(
                            &turn_context,
                            EventMsg::Warning(WarningEvent {
                                message: format!("automatic verify_local plan failed: {err:?}"),
                            }),
                        )
                        .await;
                    }
                    if let Some(status) = sess.services.task_evidence.inspect_status().await {
                        if status.phase == TaskPhase::Reviewing {
                            let review_packet =
                                match sess.services.task_evidence.prepare_review().await {
                                    Ok(review_packet) => review_packet,
                                    Err(err) => {
                                        if task_review_was_superseded_by_steering(sess.as_ref())
                                            .await
                                        {
                                            abort_pending_managed_final_reservation(
                                                sess.as_ref(),
                                                turn_context.as_ref(),
                                                &mut pending_managed_final,
                                            )
                                            .await?;
                                            last_agent_message = None;
                                            continue;
                                        }
                                        abort_pending_managed_final_reservation(
                                            sess.as_ref(),
                                            turn_context.as_ref(),
                                            &mut pending_managed_final,
                                        )
                                        .await?;
                                        return Err(CodexErr::InvalidRequest(format!(
                                            "independent review preparation failed closed: {err}"
                                        )));
                                    }
                                };
                            if let Some(review_packet) = review_packet {
                                let review_status = match crate::tasks::run_task_evidence_review(
                                    Arc::clone(&sess),
                                    Arc::clone(&turn_extension_data),
                                    Arc::clone(&turn_context),
                                    review_packet,
                                    cancellation_token.child_token(),
                                )
                                .await
                                {
                                    Ok(review_status) => review_status,
                                    Err(err) => {
                                        if task_review_was_superseded_by_steering(sess.as_ref())
                                            .await
                                        {
                                            abort_pending_managed_final_reservation(
                                                sess.as_ref(),
                                                turn_context.as_ref(),
                                                &mut pending_managed_final,
                                            )
                                            .await?;
                                            last_agent_message = None;
                                            continue;
                                        }
                                        abort_pending_managed_final_reservation(
                                            sess.as_ref(),
                                            turn_context.as_ref(),
                                            &mut pending_managed_final,
                                        )
                                        .await?;
                                        return Err(CodexErr::InvalidRequest(format!(
                                            "independent review acceptance failed closed: {err}"
                                        )));
                                    }
                                };
                                if review_status.is_none() {
                                    abort_pending_managed_final_reservation(
                                        sess.as_ref(),
                                        turn_context.as_ref(),
                                        &mut pending_managed_final,
                                    )
                                    .await?;
                                    return Err(CodexErr::TurnAborted);
                                }
                            } else {
                                sess.record_conversation_items(
                                    &turn_context,
                                    &[ResponseItem::Message {
                                        id: Some(uuid::Uuid::now_v7().to_string()),
                                        role: "user".to_string(),
                                        content: vec![ContentItem::InputText {
                                            text: "Runtime lifecycle gate: repository drift invalidated closure before independent review. Re-evaluate the current state and submit fresh closure evidence.".to_string(),
                                        }],
                                        phase: None,
                                        internal_chat_message_metadata_passthrough: None,
                                    }],
                                )
                                .await;
                            }
                            abort_pending_managed_final_reservation(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                &mut pending_managed_final,
                            )
                            .await?;
                            last_agent_message = None;
                            continue;
                        }
                        let classification_required =
                            status.phase == TaskPhase::Unclassified && status.mutation_revision > 0;
                        let closure_required = task_lifecycle_requires_closure(&status);
                        let investigation_required = status.phase == TaskPhase::Investigating;
                        if closure_required || investigation_required {
                            let instruction = if classification_required {
                                "Runtime lifecycle gate: this task has mutation evidence but is not classified. Call `task_state.classify` before any further mutation, then continue toward fresh closure."
                            } else if investigation_required {
                                "Runtime lifecycle gate: submit one batched investigation checkpoint with `task_state.submit_investigation_checkpoint`, then continue implementation. Do not finalize yet."
                            } else {
                                "Runtime lifecycle gate: the current mutation revision is not ready. Repair actionable findings or submit fresh post-mutation closure evidence with `task_state.submit_closure`. Do not repeat unchanged evidence after the recovery allowance."
                            };
                            sess.record_conversation_items(
                                &turn_context,
                                &[ResponseItem::Message {
                                    id: Some(uuid::Uuid::now_v7().to_string()),
                                    role: "user".to_string(),
                                    content: vec![ContentItem::InputText {
                                        text: instruction.to_string(),
                                    }],
                                    phase: None,
                                    internal_chat_message_metadata_passthrough: None,
                                }],
                            )
                            .await;
                            abort_pending_managed_final_reservation(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                &mut pending_managed_final,
                            )
                            .await?;
                            last_agent_message = None;
                            continue;
                        }
                    }
                    let provisional_final_pending = pending_managed_final.is_some();
                    let precommit_hook_message = precommit_hook_message(
                        last_agent_message.clone(),
                        provisional_final_pending,
                    );
                    let stop_outcome = run_turn_stop_hooks(
                        &sess,
                        &turn_context,
                        stop_hook_active,
                        precommit_hook_message.clone(),
                        provisional_final_pending,
                    )
                    .await;
                    if stop_outcome.should_block {
                        if let Some(hook_prompt_message) =
                            build_hook_prompt_message(&stop_outcome.continuation_fragments)
                        {
                            abort_pending_managed_final_reservation(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                &mut pending_managed_final,
                            )
                            .await?;
                            sess.record_response_item_and_emit_turn_item(
                                &turn_context,
                                hook_prompt_message,
                            )
                            .await;
                            stop_hook_active = true;
                            last_agent_message = None;
                            continue;
                        } else {
                            sess.send_event(
                                &turn_context,
                                EventMsg::Warning(WarningEvent {
                                    message: "Stop hook requested continuation without a prompt; ignoring the block.".to_string(),
                                }),
                            )
                            .await;
                        }
                    }
                    if !stop_outcome.should_stop
                        && run_legacy_after_agent_hook(
                            &sess,
                            &turn_context,
                            &sampling_request_input,
                            precommit_hook_message,
                        )
                        .await
                    {
                        abort_pending_managed_final_reservation(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            &mut pending_managed_final,
                        )
                        .await?;
                        return Ok(None);
                    }
                    if cancellation_token.is_cancelled() {
                        abort_pending_managed_final_reservation(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            &mut pending_managed_final,
                        )
                        .await?;
                        return Err(CodexErr::TurnAborted);
                    }
                    if let Some(pending_final) = pending_managed_final.take() {
                        match commit_and_emit_pending_managed_final(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            pending_final,
                        )
                        .await?
                        {
                            PendingManagedFinalOutcome::Emitted(message) => {
                                last_agent_message = message;
                                managed_final_committed = true;
                            }
                            PendingManagedFinalOutcome::PendingInput => {
                                last_agent_message = None;
                                continue;
                            }
                            PendingManagedFinalOutcome::Rejected => {
                                sess.record_conversation_items(
                                    &turn_context,
                                    &[ResponseItem::Message {
                                        id: Some(uuid::Uuid::now_v7().to_string()),
                                        role: "user".to_string(),
                                        content: vec![ContentItem::InputText {
                                            text: "Runtime lifecycle gate: repository or evidence state changed after the final candidate was buffered. Re-evaluate the current state and produce a fresh final only after closure is ready.".to_string(),
                                        }],
                                        phase: None,
                                        internal_chat_message_metadata_passthrough: None,
                                    }],
                                )
                                .await;
                                last_agent_message = None;
                                continue;
                            }
                        }
                    }
                    if let Some(warning) = sess
                        .services
                        .task_evidence
                        .take_finalization_warning()
                        .await
                    {
                        sess.send_event(
                            &turn_context,
                            EventMsg::Warning(WarningEvent { message: warning }),
                        )
                        .await;
                    }
                    await_models_refresh_task(&mut models_refresh_task).await;
                    if cancellation_token.is_cancelled() {
                        return Err(CodexErr::TurnAborted);
                    }
                    if !managed_final_committed
                        && sess.input_queue.has_pending_input(&sess.active_turn).await
                    {
                        last_agent_message = None;
                        continue;
                    }
                    if stop_outcome.should_stop {
                        break;
                    }
                    break;
                }
                await_models_refresh_task(&mut models_refresh_task).await;
                if cancellation_token.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                continue;
            }
            Ok(RunSamplingRequestOutcome::NeedsCompaction) => {
                if cancellation_token.is_cancelled() {
                    return Err(CodexErr::TurnAborted);
                }
                if context_limit_compaction_attempted {
                    let err = CodexErr::ContextWindowExceeded;
                    let error = err.to_codex_protocol_error();
                    sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                        .await;
                    sess.track_turn_codex_error(turn_context.as_ref(), &err);
                    sess.send_event(
                        &turn_context,
                        EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
                    )
                    .await;
                    break;
                }
                let compaction_phase = if has_sampled {
                    CompactionPhase::MidTurn
                } else {
                    CompactionPhase::PreTurn
                };
                if let Err(err) = run_auto_compact(
                    &sess,
                    Arc::clone(&step_context),
                    /*fallback_step_context*/ None,
                    &mut client_session,
                    InitialContextInjection::BeforeLastUserMessage(Arc::clone(&world_state)),
                    CompactionReason::ContextLimit,
                    compaction_phase,
                )
                .await
                {
                    if matches!(err, CodexErr::TurnAborted) {
                        return Err(err);
                    }
                    let error = err.to_codex_protocol_error();
                    sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                        .await;
                    return Ok(None);
                }
                client_session.invalidate_incremental_history("prompt-limit compaction");
                can_drain_pending_input = !model_continuation_pending;
                context_limit_compaction_attempted = true;
                continue;
            }
            Err(err @ CodexErr::TurnAborted) => {
                return Err(err);
            }
            Err(codex_error @ CodexErr::InvalidImageRequest()) => {
                let replaced_invalid_image = {
                    let mut state = sess.state.lock().await;
                    error_or_panic(
                        "Invalid image detected; sanitizing tool output to prevent poisoning",
                    );
                    state.history.replace_last_turn_images("Invalid image")
                };
                if replaced_invalid_image {
                    client_session.invalidate_incremental_history("invalid image sanitization");
                    continue;
                }

                sess.track_turn_codex_error(turn_context.as_ref(), &codex_error);
                let error = CodexErrorInfo::BadRequest;
                sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                    .await;
                let event = EventMsg::Error(ErrorEvent {
                    message: "Invalid image in your last message. Please remove it and try again."
                        .to_string(),
                    codex_error_info: Some(error),
                });
                sess.send_event(&turn_context, event).await;
                break;
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let error = e.to_codex_protocol_error();
                sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                    .await;
                sess.track_turn_codex_error(turn_context.as_ref(), &e);
                let event = EventMsg::Error(e.to_error_event(/*message_prefix*/ None));
                sess.send_event(&turn_context, event).await;
                // let the user continue the conversation
                break;
            }
        }
    }

    Ok(last_agent_message)
}

fn task_lifecycle_requires_closure(status: &TaskLifecycleStatus) -> bool {
    status.phase == TaskPhase::Fixing
        || ((status.mutation_revision > 0 || !status.unsupported_mutation_targets.is_empty())
            && status.phase != TaskPhase::Ready)
}

async fn task_review_was_superseded_by_steering(sess: &Session) -> bool {
    if !sess.input_queue.has_pending_input(&sess.active_turn).await {
        return false;
    }
    sess.services
        .task_evidence
        .inspect_status()
        .await
        .is_some_and(|status| status.phase != TaskPhase::Reviewing)
}

#[instrument(level = "trace", skip_all)]
async fn turn_diff_display_roots(
    sess: &Session,
    turn_context: &TurnContext,
) -> Vec<(String, PathBuf)> {
    sess.services
        .git_workspace
        .snapshot(&turn_context.environments)
        .await
        .display_roots()
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn run_hooks_and_record_inputs(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
) -> bool {
    let mut blocked_input = false;
    let mut accepted_user_input = false;
    for input_item in input {
        let hook_outcome = inspect_pending_input(sess, turn_context, input_item).await;
        if hook_outcome.should_stop {
            blocked_input = true;
            record_additional_contexts(sess, turn_context, hook_outcome.additional_contexts).await;
        } else {
            if matches!(input_item, TurnInput::UserInput { content, .. } if !content.is_empty()) {
                accepted_user_input = true;
            }
            record_pending_input(
                sess,
                turn_context,
                input_item.clone(),
                hook_outcome.additional_contexts,
            )
            .await;
        }
    }
    blocked_input && !accepted_user_input
}

struct PendingTurnPlan {
    identity: PlanningSnapshotIdentity,
    step_context: Arc<StepContext>,
    first_router: Arc<ToolRouter>,
    injection_items: Vec<ResponseItem>,
    explicitly_enabled_connectors: HashSet<String>,
    pending_token_estimate: i64,
    mcp_dependency_effect: Option<PlannedMcpDependencyEffect>,
    warnings: Vec<String>,
    skill_plan: PlannedSkillInjections,
    tracking: TrackEventsContext,
    mentioned_apps: Vec<(String, Option<String>)>,
    mentioned_plugins: Vec<PluginCapabilitySummary>,
    pre_sampling_context_limit_compaction_completed: bool,
}

enum PendingTurnPlanBuild {
    Stale,
    Ready(Box<PendingTurnPlan>),
}

#[instrument(level = "trace", skip_all)]
async fn build_pure_pending_turn_plan(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    input: &[TurnInput],
    cancellation_token: &CancellationToken,
) -> CodexResult<PendingTurnPlanBuild> {
    let turn_context = step_context.turn.as_ref();
    let generation = sess.services.planning_generation();
    let user_input = input
        .iter()
        .filter_map(|item| match item {
            TurnInput::UserInput { content, .. } => Some(content.as_slice()),
            TurnInput::ResponseItem(_) | TurnInput::InterAgentCommunication(_) => None,
        })
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let tracking = build_track_events_context(
        turn_context.model_info.slug.clone(),
        sess.thread_id.to_string(),
        turn_context.sub_id.clone(),
        turn_context.originator.clone(),
    );

    if crate::guardian::is_guardian_reviewer_source(&turn_context.session_source) {
        let (history_items, first_router) = tokio::join!(
            async {
                sess.clone_history()
                    .await
                    .for_prompt(&turn_context.model_info.input_modalities)
            },
            built_tools(sess.as_ref(), step_context.as_ref(), cancellation_token)
        );
        let first_router = first_router?;
        let identity = PlanningSnapshotIdentity {
            generation,
            state_digest: planning_state_digest(PlanningStateDigestInput {
                step_context: step_context.as_ref(),
                mcp_tools: &[],
                connectors: &[],
                plugins: &[],
                injection_items: &[],
                user_input: &user_input,
                router: first_router.as_ref(),
                history_items: &history_items,
            }),
        };
        return Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
            identity,
            step_context,
            first_router,
            injection_items: Vec::new(),
            explicitly_enabled_connectors: HashSet::new(),
            pending_token_estimate: estimate_pending_tokens(input, &[]),
            mcp_dependency_effect: None,
            warnings: Vec::new(),
            skill_plan: PlannedSkillInjections::default(),
            tracking,
            mentioned_apps: Vec::new(),
            mentioned_plugins: Vec::new(),
            pre_sampling_context_limit_compaction_completed: false,
        })));
    }

    // Read-only DAG roots P and E are independent. Extension contributors poll
    // concurrently internally and collect by registration index.
    let plugins_config_input = turn_context.config.plugins_config_input();
    let (loaded_plugins, extension_injection_items) = tokio::join!(
        sess.services
            .plugins_manager
            .plugins_for_config(&plugins_config_input),
        build_extension_turn_input_items(sess, turn_context, &user_input, cancellation_token)
    );
    let extension_injection_items = extension_injection_items?;
    // DAG edge P -> plugin mentions. Connector inventory C waits for P because
    // plugin mentions can make inventory necessary even when apps are disabled.
    let mentioned_plugins =
        collect_explicit_plugin_mentions(&user_input, loaded_plugins.capability_summaries());
    let connector_snapshot = step_context.mcp.config().connector_snapshot.clone();
    let mcp_tools = if turn_context.apps_enabled() || !mentioned_plugins.is_empty() {
        step_context
            .mcp_tools()
            .or_cancel(cancellation_token)
            .await?
    } else {
        &[]
    };
    let available_connectors = if turn_context.apps_enabled() {
        let connectors = codex_connectors::merge::merge_plugin_connectors_with_accessible(
            connector_snapshot
                .connector_ids()
                .iter()
                .map(|connector_id| connector_id.0.clone()),
            connectors::accessible_connectors_from_mcp_tools(mcp_tools),
        );
        connectors::with_app_enabled_state(connectors, &turn_context.config)
    } else {
        Vec::new()
    };

    // C -> SK: connector names disambiguate plaintext skill mentions.
    let skills_outcome = turn_context.turn_skills.snapshot.outcome();
    let connector_slug_counts = build_connector_slug_counts(&available_connectors);
    let skill_name_counts_lower =
        build_skill_name_counts(&skills_outcome.skills, &skills_outcome.disabled_paths).1;
    let mentioned_skills = collect_explicit_skill_mentions(
        &user_input,
        &skills_outcome.skills,
        &skills_outcome.disabled_paths,
        &connector_slug_counts,
    );
    // Once SK is resolved, inventory-effect planning and pure skill materialization
    // are independent and remain side-effect free.
    let (planned_mcp, skill_plan) = tokio::join!(
        plan_mcp_dependencies(sess, turn_context, &mentioned_skills),
        plan_skill_injections(&mentioned_skills, Some(skills_outcome))
    );
    let skill_items = skill_plan
        .injections
        .items
        .iter()
        .map(|skill| ContextualUserFragment::into(crate::context::SkillInstructions::from(skill)))
        .collect::<Vec<ResponseItem>>();
    let skill_connector_ids = collect_explicit_app_ids_from_skill_items(
        &skill_items,
        &available_connectors,
        &skill_name_counts_lower,
    );
    let plugin_items = build_plugin_injections(
        &mentioned_plugins,
        mcp_tools,
        &available_connectors,
        &step_context.mcp.config().mcp_server_catalog,
        &connector_snapshot,
    );
    let mut explicitly_enabled_connectors = collect_explicit_app_ids(&user_input);
    explicitly_enabled_connectors.extend(skill_connector_ids);
    let connector_names_by_id = available_connectors
        .iter()
        .map(|connector| (connector.id.as_str(), connector.name.as_str()))
        .collect::<HashMap<_, _>>();
    let mentioned_apps = explicitly_enabled_connectors
        .iter()
        .map(|connector_id| {
            (
                connector_id.clone(),
                connector_names_by_id
                    .get(connector_id.as_str())
                    .map(|name| (*name).to_string()),
            )
        })
        .collect::<Vec<_>>();
    let mut mentioned_apps = mentioned_apps;
    mentioned_apps.sort_by(|left, right| left.0.cmp(&right.0));

    let injected_host_skill_prompts = turn_context
        .extension_data
        .get::<InjectedHostSkillPrompts>();
    let mut injection_items = match injected_host_skill_prompts {
        Some(injected_host_skill_prompts) => skill_plan
            .injections
            .items
            .iter()
            .filter(|skill| !injected_host_skill_prompts.contains_path(&skill.path))
            .map(|skill| {
                ContextualUserFragment::into(crate::context::SkillInstructions::from(skill))
            })
            .collect::<Vec<_>>(),
        None => skill_items,
    };
    injection_items.extend(plugin_items);
    injection_items.extend(extension_injection_items);

    // Final read-only DAG leaves: capture the model-visible history and build the
    // router concurrently, then validate the generation before accepting either.
    let (history_items, first_router) = tokio::join!(
        async {
            sess.clone_history()
                .await
                .for_prompt(&turn_context.model_info.input_modalities)
        },
        built_tools(sess.as_ref(), step_context.as_ref(), cancellation_token)
    );
    let first_router = first_router?;
    if sess.services.planning_generation() != generation {
        return Ok(PendingTurnPlanBuild::Stale);
    }
    let mut warnings = planned_mcp.warnings;
    warnings.extend(skill_plan.injections.warnings.iter().cloned());
    let identity = PlanningSnapshotIdentity {
        generation,
        state_digest: planning_state_digest(PlanningStateDigestInput {
            step_context: step_context.as_ref(),
            mcp_tools,
            connectors: &available_connectors,
            plugins: &mentioned_plugins,
            injection_items: &injection_items,
            user_input: &user_input,
            router: first_router.as_ref(),
            history_items: &history_items,
        }),
    };
    Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
        identity,
        step_context,
        first_router,
        pending_token_estimate: estimate_pending_tokens(input, &injection_items),
        injection_items,
        explicitly_enabled_connectors,
        mcp_dependency_effect: planned_mcp.effect,
        warnings,
        skill_plan,
        tracking,
        mentioned_apps,
        mentioned_plugins,
        pre_sampling_context_limit_compaction_completed: false,
    })))
}

async fn stabilize_pending_turn_plan(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
    client_session: &mut ModelClientSession,
    cancellation_token: &CancellationToken,
) -> CodexResult<PendingTurnPlan> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if !turn_context
        .config
        .features
        .enabled(Feature::DeferredExecutor)
    {
        // Normal turns freeze their environment selection, so refresh project instructions once
        // at the turn boundary. Fixed-point retries reuse this published snapshot.
        sess.services
            .agents_md_manager
            .refresh(&turn_context.config, &turn_context.environments)
            .await;
    }

    let mut fixed_point = FixedPointPlanningState::default();
    let mut check_previous_model_compaction = true;
    let mut incoming_precompaction_completed = false;
    let mut pre_sampling_context_limit_compaction_completed = false;
    loop {
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        fixed_point.begin_attempt().map_err(planning_failure)?;

        let completed_inventory_effects = fixed_point
            .completed_inventory_effects()
            .map(|(id, effect)| (id.to_string(), effect.expected_inventory_keys.clone()))
            .collect::<Vec<_>>();
        for (effect_id, expected_inventory_keys) in completed_inventory_effects {
            if !inventory_contains_expected(sess, &expected_inventory_keys).await {
                return Err(planning_failure(format!(
                    "completed inventory effect `{effect_id}` is missing its expected model-visible state"
                )));
            }
        }

        let step_context = sess.capture_step_context(Arc::clone(turn_context)).await;
        let plan = match build_pure_pending_turn_plan(sess, step_context, input, cancellation_token)
            .await?
        {
            PendingTurnPlanBuild::Stale => continue,
            PendingTurnPlanBuild::Ready(plan) => *plan,
        };
        fixed_point
            .observe_snapshot(&plan.identity)
            .map_err(planning_failure)?;

        // Compaction is maintenance of the pre-existing history, not persistence
        // of this pending turn. Any compaction invalidates this pure plan and loops
        // through a fresh snapshot before effects are applied.
        let compaction_timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Compaction);
        let compaction_reason = run_pre_sampling_compact(
            sess,
            turn_context,
            client_session,
            check_previous_model_compaction,
            plan.pending_token_estimate,
            !incoming_precompaction_completed,
            !pre_sampling_context_limit_compaction_completed,
        )
        .await?;
        drop(compaction_timing_guard);
        check_previous_model_compaction = false;
        match compaction_reason {
            Some(PreSamplingCompactionReason::PendingInputLimit) => {
                incoming_precompaction_completed = true;
                pre_sampling_context_limit_compaction_completed = true;
            }
            Some(PreSamplingCompactionReason::CommittedHistoryLimit) => {
                pre_sampling_context_limit_compaction_completed = true;
            }
            Some(PreSamplingCompactionReason::PreviousModel) | None => {}
        }
        if compaction_reason.is_some() {
            client_session.invalidate_incremental_history("compaction");
            sess.services.bump_planning_generation();
            continue;
        }

        if let Some(effect) = plan.mcp_dependency_effect.as_ref()
            && fixed_point.completed(&effect.id).is_none()
        {
            let outcome = apply_mcp_dependency_effect(
                sess,
                turn_context,
                cancellation_token,
                effect,
                Some(sess.mcp_elicitation_reviewer()),
            )
            .await
            .map_err(|err| {
                if cancellation_token.is_cancelled() {
                    CodexErr::TurnAborted
                } else {
                    planning_failure(format!("effect `{}` failed: {err}", effect.id))
                }
            })?;
            let completed = match outcome {
                McpDependencyEffectOutcome::Skipped => CompletedEffect {
                    impact: EffectImpact::NonInvalidating,
                    expected_inventory_keys: HashSet::new(),
                },
                McpDependencyEffectOutcome::InventoryChanged {
                    expected_inventory_keys,
                } => CompletedEffect {
                    impact: EffectImpact::InvalidatesInventory,
                    expected_inventory_keys,
                },
            };
            let impact = completed.impact;
            fixed_point
                .record_completed(effect.id.clone(), completed)
                .map_err(planning_failure)?;
            if impact.invalidates_snapshot() {
                client_session.invalidate_incremental_history("model-visible planning effect");
                fixed_point
                    .require_generation_advance(
                        &plan.identity,
                        sess.services.planning_generation(),
                        impact,
                    )
                    .map_err(planning_failure)?;
                continue;
            }
        }

        for message in &plan.warnings {
            let effect_id = semantic_effect_id("warning", std::slice::from_ref(message));
            if fixed_point.completed(&effect_id).is_some() {
                continue;
            }
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: message.clone(),
                }),
            )
            .await;
            fixed_point
                .record_completed(
                    effect_id,
                    CompletedEffect {
                        impact: EffectImpact::NonInvalidating,
                        expected_inventory_keys: HashSet::new(),
                    },
                )
                .map_err(planning_failure)?;
        }

        let skill_effect_values = plan
            .skill_plan
            .invocations
            .iter()
            .map(|invocation| {
                format!(
                    "{}:{}:{}",
                    invocation.skill_name,
                    invocation.skill_path.display(),
                    invocation
                        .plugin_id
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                )
            })
            .chain(
                plan.skill_plan
                    .metrics
                    .iter()
                    .map(|metric| format!("{}:{}", metric.skill_name, metric.status)),
            )
            .collect::<Vec<_>>();
        if !skill_effect_values.is_empty() {
            let effect_id = semantic_effect_id("skill_observability", &skill_effect_values);
            if fixed_point.completed(&effect_id).is_none() {
                apply_skill_injection_observability(
                    &plan.skill_plan,
                    Some(&turn_context.session_telemetry),
                    &sess.services.analytics_events_client,
                    plan.tracking.clone(),
                );
                fixed_point
                    .record_completed(
                        effect_id,
                        CompletedEffect {
                            impact: EffectImpact::NonInvalidating,
                            expected_inventory_keys: HashSet::new(),
                        },
                    )
                    .map_err(planning_failure)?;
            }
        }

        let app_effect_values = plan
            .mentioned_apps
            .iter()
            .map(|(id, name)| format!("{id}:{}", name.as_deref().unwrap_or_default()))
            .collect::<Vec<_>>();
        if !app_effect_values.is_empty() {
            let effect_id = semantic_effect_id("app_analytics", &app_effect_values);
            if fixed_point.completed(&effect_id).is_none() {
                let invocations = plan
                    .mentioned_apps
                    .iter()
                    .map(|(connector_id, app_name)| AppInvocation {
                        connector_id: Some(connector_id.clone()),
                        app_name: app_name.clone(),
                        invocation_type: Some(InvocationType::Explicit),
                    })
                    .collect();
                sess.services
                    .analytics_events_client
                    .track_app_mentioned(plan.tracking.clone(), invocations);
                fixed_point
                    .record_completed(
                        effect_id,
                        CompletedEffect {
                            impact: EffectImpact::NonInvalidating,
                            expected_inventory_keys: HashSet::new(),
                        },
                    )
                    .map_err(planning_failure)?;
            }
        }

        let plugin_effect_values = plan
            .mentioned_plugins
            .iter()
            .map(|plugin| plugin.config_name.clone())
            .collect::<Vec<_>>();
        if !plugin_effect_values.is_empty() {
            let effect_id = semantic_effect_id("plugin_analytics", &plugin_effect_values);
            if fixed_point.completed(&effect_id).is_none() {
                for summary in &plan.mentioned_plugins {
                    if let Some(plugin) = sess
                        .services
                        .plugins_manager
                        .telemetry_metadata_for_capability_summary(summary)
                    {
                        sess.services
                            .analytics_events_client
                            .track_plugin_used(plan.tracking.clone(), plugin);
                    }
                }
                fixed_point
                    .record_completed(
                        effect_id,
                        CompletedEffect {
                            impact: EffectImpact::NonInvalidating,
                            expected_inventory_keys: HashSet::new(),
                        },
                    )
                    .map_err(planning_failure)?;
            }
        }

        if sess.services.planning_generation() != plan.identity.generation {
            continue;
        }
        let mut plan = plan;
        plan.pre_sampling_context_limit_compaction_completed =
            pre_sampling_context_limit_compaction_completed;
        return Ok(plan);
    }
}

fn planning_failure(message: impl Into<String>) -> CodexErr {
    CodexErr::Stream(
        format!("pending-turn planning failure: {}", message.into()),
        None,
    )
}

fn semantic_effect_id(kind: &str, values: &[String]) -> String {
    let mut values = values.to_vec();
    values.sort();
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    for value in values {
        hasher.update([0]);
        hasher.update(value.as_bytes());
    }
    format!("{kind}:{:x}", hasher.finalize())
}

struct PlanningStateDigestInput<'a> {
    step_context: &'a StepContext,
    mcp_tools: &'a [codex_mcp::ToolInfo],
    connectors: &'a [connectors::AppInfo],
    plugins: &'a [PluginCapabilitySummary],
    injection_items: &'a [ResponseItem],
    user_input: &'a [UserInput],
    router: &'a ToolRouter,
    history_items: &'a [ResponseItem],
}

fn planning_state_digest(input: PlanningStateDigestInput<'_>) -> String {
    let PlanningStateDigestInput {
        step_context,
        mcp_tools,
        connectors,
        plugins,
        injection_items,
        user_input,
        router,
        history_items,
    } = input;
    let mut hasher = Sha256::new();
    for environment in &step_context.environments.turn_environments {
        hasher.update(environment.environment_id.as_bytes());
        hasher.update(environment.cwd().to_string().as_bytes());
    }
    for starting in &step_context.environments.starting {
        hasher.update(format!("{:?}", starting.selection).as_bytes());
    }
    hasher.update(format!("{:?}", step_context.selected_capability_roots).as_bytes());
    hasher.update(format!("{:?}", step_context.loaded_agents_md).as_bytes());
    for plugin in plugins {
        hasher.update(plugin.config_name.as_bytes());
        hasher.update(plugin.display_name.as_bytes());
        for server in &plugin.mcp_server_names {
            hasher.update(server.as_bytes());
        }
        for connector in &plugin.app_connector_ids {
            hasher.update(connector.0.as_bytes());
        }
    }
    for serialized in [
        serde_json::to_vec(mcp_tools),
        serde_json::to_vec(connectors),
        serde_json::to_vec(injection_items),
        serde_json::to_vec(user_input),
        serde_json::to_vec(&router.model_visible_specs()),
        serde_json::to_vec(history_items),
    ]
    .into_iter()
    .flatten()
    {
        hasher.update(serialized);
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn estimate_pending_tokens(input: &[TurnInput], injection_items: &[ResponseItem]) -> i64 {
    let input_bytes = input.iter().fold(0usize, |bytes, item| {
        let item_bytes = match item {
            TurnInput::UserInput { content, .. } => serde_json::to_vec(content),
            TurnInput::ResponseItem(item) => serde_json::to_vec(item),
            TurnInput::InterAgentCommunication(communication) => {
                serde_json::to_vec(&communication.to_model_input_item())
            }
        }
        .map(|value| value.len())
        .unwrap_or_default();
        bytes.saturating_add(item_bytes)
    });
    let bytes = input_bytes.saturating_add(
        serde_json::to_vec(injection_items)
            .map(|value| value.len())
            .unwrap_or_default(),
    );
    i64::try_from(bytes.div_ceil(4)).unwrap_or(i64::MAX)
}

pub(crate) fn task_contract_from_input(input: &[TurnInput]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            TurnInput::UserInput { content, .. } => {
                Some(task_contract_from_user_input(content.as_slice()))
            }
            TurnInput::InterAgentCommunication(communication) => {
                serde_json::to_string(&communication.to_model_input_item()).ok()
            }
            TurnInput::ResponseItem(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn task_contract_from_inter_agent_input(input: &[TurnInput]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            TurnInput::InterAgentCommunication(communication) => {
                serde_json::to_string(&communication.to_model_input_item()).ok()
            }
            TurnInput::UserInput { .. } | TurnInput::ResponseItem(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn task_contract_from_user_input(input: &[UserInput]) -> String {
    serde_json::to_string(input).unwrap_or_default()
}

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(user_input_count = user_input.len())
)]
async fn build_extension_turn_input_items(
    sess: &Arc<Session>,
    turn_context: &TurnContext,
    user_input: &[UserInput],
    cancellation_token: &CancellationToken,
) -> CodexResult<Vec<ResponseItem>> {
    let contributors = sess.services.extensions.turn_input_contributors().to_vec();
    if contributors.is_empty() {
        return Ok(Vec::new());
    }

    let environments = turn_context
        .environments
        .turn_environments
        .iter()
        .enumerate()
        .filter_map(|(index, environment)| {
            // TODO(anp): Migrate extension turn-input environments to PathUri so foreign cwd
            // values are not omitted from extension context.
            Some(TurnInputEnvironment {
                environment_id: environment.environment_id.clone(),
                cwd: environment.cwd().to_abs_path().ok()?.into_path_buf(),
                is_primary: index == 0,
            })
        })
        .collect::<Vec<_>>();

    let input = TurnInputContext {
        turn_id: turn_context.sub_id.to_string(),
        user_input: user_input.to_vec(),
        environments,
    };

    // Contributors are independent read-only DAG leaves. FuturesOrdered polls
    // them concurrently while preserving registration order in the result.
    let mut pending = FuturesOrdered::new();
    let session_extension_data = &sess.services.session_extension_data;
    let thread_extension_data = &sess.services.thread_extension_data;
    let turn_extension_data = turn_context.extension_data.as_ref();
    for contributor in contributors {
        let input = input.clone();
        pending.push_back(async move {
            contributor
                .contribute(
                    input,
                    session_extension_data,
                    thread_extension_data,
                    turn_extension_data,
                )
                .or_cancel(cancellation_token)
                .await
        });
    }
    let mut items = Vec::new();
    while let Some(contributed_fragments) = pending.next().await {
        let contributed_fragments = contributed_fragments?;
        items.extend(
            contributed_fragments
                .into_iter()
                .map(ContextualUserFragment::into_boxed_response_item),
        );
    }

    Ok(items)
}

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(input_count = input.len())
)]
async fn track_turn_resolved_config_analytics(
    sess: &Session,
    turn_context: &TurnContext,
    input: &[TurnInput],
) {
    let thread_config = {
        let state = sess.state.lock().await;
        state.session_configuration.thread_config_snapshot()
    };
    let is_first_turn = {
        let mut state = sess.state.lock().await;
        state.take_next_turn_is_first()
    };
    sess.services
        .analytics_events_client
        .track_turn_resolved_config(TurnResolvedConfigFact {
            turn_id: turn_context.sub_id.clone(),
            thread_id: sess.thread_id.to_string(),
            num_input_images: input
                .iter()
                .filter_map(|item| match item {
                    TurnInput::UserInput { content, .. } => Some(content.as_slice()),
                    TurnInput::ResponseItem(_) | TurnInput::InterAgentCommunication(_) => None,
                })
                .flatten()
                .filter(|item| {
                    matches!(item, UserInput::Image { .. } | UserInput::LocalImage { .. })
                })
                .count(),
            submission_type: None,
            ephemeral: thread_config.ephemeral,
            session_source: thread_config.session_source,
            model: turn_context.model_info.slug.clone(),
            model_provider: turn_context.config.model_provider_id.clone(),
            permission_profile: turn_context.permission_profile(),
            #[allow(deprecated)]
            permission_profile_cwd: turn_context.cwd.to_path_buf(),
            reasoning_effort: turn_context.reasoning_effort.clone(),
            reasoning_summary: Some(turn_context.reasoning_summary),
            service_tier: turn_context
                .config
                .service_tier
                .as_deref()
                .and_then(ServiceTier::from_request_value),
            approval_policy: turn_context.approval_policy.value(),
            approvals_reviewer: turn_context.config.approvals_reviewer,
            sandbox_network_access: turn_context.network_sandbox_policy().is_enabled(),
            collaboration_mode: turn_context.collaboration_mode.mode,
            personality: turn_context.personality,
            workspace_kind: turn_context.turn_metadata_state.workspace_kind(),
            is_first_turn,
        });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreSamplingCompactionReason {
    PreviousModel,
    CommittedHistoryLimit,
    PendingInputLimit,
}

#[instrument(level = "trace", skip_all)]
async fn run_pre_sampling_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    check_previous_model: bool,
    pending_token_estimate: i64,
    allow_pending_input_compaction: bool,
    allow_context_limit_compaction: bool,
) -> CodexResult<Option<PreSamplingCompactionReason>> {
    if check_previous_model
        && maybe_run_previous_model_inline_compact(sess, turn_context, client_session).await?
    {
        return Ok(Some(PreSamplingCompactionReason::PreviousModel));
    }
    // A completed context-limit compaction already addressed the current history
    // snapshot. Re-evaluating cumulative token usage here can report the same
    // soft limit again even though there is no second useful pass to perform.
    if !allow_context_limit_compaction {
        return Ok(None);
    }
    let token_status =
        super::context_window::context_window_token_status(sess.as_ref(), turn_context.as_ref())
            .await;
    // Include the pure plan's incoming user/plugin/skill contribution. This
    // closes the old gap where pre-turn compaction considered only committed history.
    let incoming_reaches_limit = allow_pending_input_compaction
        && token_status
            .tokens_until_compaction
            .is_some_and(|remaining| pending_token_estimate >= remaining);
    let compaction_reason = if incoming_reaches_limit {
        Some(PreSamplingCompactionReason::PendingInputLimit)
    } else if token_status.token_limit_reached {
        Some(PreSamplingCompactionReason::CommittedHistoryLimit)
    } else {
        None
    };
    if let Some(compaction_reason) = compaction_reason {
        // Pre-turn compaction runs before run_turn creates the normal sampling step.
        let step_context = sess.capture_step_context(Arc::clone(turn_context)).await;
        run_auto_compact(
            sess,
            step_context,
            /*fallback_step_context*/ None,
            client_session,
            InitialContextInjection::DoNotInject,
            CompactionReason::ContextLimit,
            CompactionPhase::PreTurn,
        )
        .await?;
        return Ok(Some(compaction_reason));
    }
    Ok(None)
}

/// Returns true only when both turns declare compaction compatibility hashes and they differ.
/// A missing hash does not provide enough information to trigger compaction.
fn comp_hash_changed(previous: Option<&str>, current: Option<&str>) -> bool {
    previous
        .zip(current)
        .is_some_and(|(previous, current)| previous != current)
}

/// Captures the current model's request-scoped state for retrying previous-model compaction.
///
/// Returns `None` when the active authentication does not use the Codex backend, the provider is
/// not OpenAI, or the previous and current model are the same.
async fn capture_current_model_fallback_step_context(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    previous_model: &str,
) -> Option<Arc<StepContext>> {
    let uses_codex_backend = turn_context
        .auth_manager
        .as_deref()
        .is_some_and(codex_login::AuthManager::current_auth_uses_codex_backend);
    if !uses_codex_backend
        || !turn_context.provider.info().is_openai()
        || previous_model == turn_context.model_info.slug
    {
        return None;
    }
    Some(sess.capture_step_context(Arc::clone(turn_context)).await)
}

/// Runs pre-sampling compaction against the previous model when its compaction compatibility
/// hash changed or when switching to a smaller context-window model.
///
/// Returns `Err(_)` only when compaction was attempted and failed.
async fn maybe_run_previous_model_inline_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
) -> CodexResult<bool> {
    let Some(previous_turn_settings) = sess.previous_turn_settings().await else {
        return Ok(false);
    };
    let should_compact_for_comp_hash_change = comp_hash_changed(
        previous_turn_settings.comp_hash.as_deref(),
        turn_context.model_info.comp_hash.as_deref(),
    );
    let previous_model = previous_turn_settings.model;
    if !should_compact_for_comp_hash_change && previous_model == turn_context.model_info.slug {
        return Ok(false);
    }
    let previous_model_turn_context = Arc::new(
        turn_context
            .with_model(previous_model.clone(), &sess.services.models_manager)
            .await,
    );

    if should_compact_for_comp_hash_change {
        let step_context = sess
            .capture_step_context(Arc::clone(&previous_model_turn_context))
            .await;
        let fallback_step_context = capture_current_model_fallback_step_context(
            sess,
            turn_context,
            previous_model.as_str(),
        )
        .await;
        run_auto_compact(
            sess,
            step_context,
            fallback_step_context,
            client_session,
            InitialContextInjection::DoNotInject,
            CompactionReason::CompHashChanged,
            CompactionPhase::PreTurn,
        )
        .await?;
        return Ok(true);
    }

    let Some(old_context_window) = previous_model_turn_context.model_context_window() else {
        return Ok(false);
    };
    let Some(new_context_window) = turn_context.model_context_window() else {
        return Ok(false);
    };
    let active_context_tokens = sess.get_total_token_usage().await;
    let previous_model_limit_reached = match turn_context
        .config
        .model_auto_compact_token_limit_scope
    {
        AutoCompactTokenLimitScope::Total => {
            let new_auto_compact_limit = turn_context
                .model_info
                .auto_compact_token_limit()
                .unwrap_or(i64::MAX);
            active_context_tokens > new_auto_compact_limit
                || active_context_tokens >= new_context_window
        }
        AutoCompactTokenLimitScope::BodyAfterPrefix => active_context_tokens >= new_context_window,
    };
    let should_run = previous_model_limit_reached
        && previous_model_turn_context.model_info.slug != turn_context.model_info.slug
        && old_context_window > new_context_window;
    if should_run {
        let step_context = sess
            .capture_step_context(Arc::clone(&previous_model_turn_context))
            .await;
        let fallback_step_context = capture_current_model_fallback_step_context(
            sess,
            turn_context,
            previous_model.as_str(),
        )
        .await;
        run_auto_compact(
            sess,
            step_context,
            fallback_step_context,
            client_session,
            InitialContextInjection::DoNotInject,
            CompactionReason::ModelDownshift,
            CompactionPhase::PreTurn,
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}

#[instrument(
    level = "trace",
    skip_all,
    fields(reason = ?reason, phase = ?phase)
)]
async fn run_auto_compact(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    client_session: &mut ModelClientSession,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    if turn_context.config.features.enabled(Feature::TokenBudget) {
        // Compaction is the reset request, so force a new context window
        // instead of consuming a pending `new_context` tool request.
        crate::compact_token_budget::run_inline_auto_compact_task(
            Arc::clone(sess),
            step_context,
            initial_context_injection,
        )
        .await?;
        return Ok(());
    }

    if should_use_remote_compact_task(turn_context.provider.info()) {
        if turn_context
            .config
            .features
            .enabled(Feature::RemoteCompactionV2)
        {
            emit_compact_metric(
                &sess.services.session_telemetry,
                "remote_v2",
                /*manual*/ false,
            );
            run_inline_remote_auto_compact_task_v2(
                Arc::clone(sess),
                step_context,
                fallback_step_context,
                client_session,
                initial_context_injection,
                reason,
                phase,
            )
            .await?;
            return Ok(());
        }
        emit_compact_metric(
            &sess.services.session_telemetry,
            "remote",
            /*manual*/ false,
        );
        run_inline_remote_auto_compact_task(
            Arc::clone(sess),
            step_context,
            fallback_step_context,
            client_session.turn_state(),
            initial_context_injection,
            reason,
            phase,
        )
        .await?;
    } else {
        emit_compact_metric(
            &sess.services.session_telemetry,
            "local",
            /*manual*/ false,
        );
        run_inline_auto_compact_task(
            Arc::clone(sess),
            Arc::clone(turn_context),
            initial_context_injection,
            reason,
            phase,
        )
        .await?;
    }
    Ok(())
}

pub(super) fn collect_explicit_app_ids_from_skill_items(
    skill_items: &[ResponseItem],
    connectors: &[connectors::AppInfo],
    skill_name_counts_lower: &HashMap<String, usize>,
) -> HashSet<String> {
    if skill_items.is_empty() || connectors.is_empty() {
        return HashSet::new();
    }

    let skill_messages = skill_items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => {
                content.iter().find_map(|content_item| match content_item {
                    ContentItem::InputText { text } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect::<Vec<String>>();
    if skill_messages.is_empty() {
        return HashSet::new();
    }

    let mentions = collect_tool_mentions_from_messages(&skill_messages);
    let mention_names_lower = mentions
        .plain_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<String>>();
    let mut connector_ids = mentions
        .paths
        .iter()
        .filter(|path| tool_kind_for_path(path) == ToolMentionKind::App)
        .filter_map(|path| app_id_from_path(path).map(str::to_string))
        .collect::<HashSet<String>>();

    let connector_slug_counts = build_connector_slug_counts(connectors);
    for connector in connectors {
        let slug = codex_connectors::metadata::connector_mention_slug(connector);
        let connector_count = connector_slug_counts.get(&slug).copied().unwrap_or(0);
        let skill_count = skill_name_counts_lower.get(&slug).copied().unwrap_or(0);
        if connector_count == 1 && skill_count == 0 && mention_names_lower.contains(&slug) {
            connector_ids.insert(connector.id.clone());
        }
    }

    connector_ids
}

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_prompt(
    input: Vec<ResponseItem>,
    router: &ToolRouter,
    turn_context: &TurnContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    Prompt {
        input,
        tools: router.model_visible_specs(),
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        base_instructions,
        output_schema: turn_context.final_output_json_schema.clone(),
        output_schema_strict: !crate::guardian::is_guardian_reviewer_source(
            &turn_context.session_source,
        ),
    }
}

enum RunSamplingRequestOutcome {
    Sampled(SamplingRequestResult, Vec<ResponseItem>),
    NeedsCompaction,
}

#[allow(clippy::too_many_arguments)]
#[allow(deprecated)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %step_context.turn.sub_id,
        model = %step_context.turn.model_info.slug,
        cwd = %step_context.turn.cwd.display()
    )
)]
async fn run_sampling_request(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    turn_store: Arc<codex_extension_api::ExtensionData>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    responses_metadata: &CodexResponsesMetadata,
    input: Vec<ResponseItem>,
    prebuilt_router: &mut Option<Arc<ToolRouter>>,
    preparation_timing_guard: &mut Option<TurnTimingGuard>,
    cancellation_token: CancellationToken,
) -> CodexResult<RunSamplingRequestOutcome> {
    let turn_context = Arc::clone(&step_context.turn);
    let router = match prebuilt_router
        .take()
        .filter(|_| crate::latency_switches::stage2_critical_path_enabled())
    {
        Some(router) => router,
        None => built_tools(sess.as_ref(), step_context.as_ref(), &cancellation_token).await?,
    };

    let base_instructions = sess.get_base_instructions().await;

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&step_context),
        Arc::clone(&turn_diff_tracker),
    );
    let _code_mode_worker = sess.services.code_mode_service.start_turn_worker(
        &sess,
        Arc::clone(&step_context),
        Arc::clone(&router),
        Arc::clone(&turn_diff_tracker),
    );
    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retries = 0;
    let mut initial_input = Some(input);
    let mut original_input = None;
    loop {
        let prompt_input = if let Some(input) = initial_input.take() {
            input
        } else {
            sess.clone_history()
                .await
                .for_prompt(&turn_context.model_info.input_modalities)
        };
        let prompt = build_prompt(
            prompt_input,
            router.as_ref(),
            turn_context.as_ref(),
            base_instructions.clone(),
        );
        let estimated_prompt_tokens =
            crate::context_manager::ContextManager::estimate_prompt_token_count(&prompt);
        if super::context_window::estimated_prompt_reaches_hard_limit(
            turn_context.as_ref(),
            estimated_prompt_tokens,
        ) {
            return Ok(RunSamplingRequestOutcome::NeedsCompaction);
        }
        let err = match try_run_sampling_request(
            tool_runtime.clone(),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_store),
            client_session,
            responses_metadata,
            Arc::clone(&turn_diff_tracker),
            &prompt,
            preparation_timing_guard,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => {
                return Ok(RunSamplingRequestOutcome::Sampled(
                    output,
                    original_input.unwrap_or(prompt.input),
                ));
            }
            Err(CodexErr::ContextWindowExceeded) => {
                sess.set_total_tokens_full(&turn_context).await;
                return Err(CodexErr::ContextWindowExceeded);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, *rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(err) => err,
        };

        if original_input.is_none() {
            original_input = Some(prompt.input);
        }

        if !err.is_retryable() {
            return Err(err);
        }

        let retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
        let retry_result = handle_retryable_response_stream_error(
            &mut retries,
            max_retries,
            err,
            client_session,
            &sess,
            &turn_context,
            ResponsesStreamRequest::Sampling,
        )
        .await;
        drop(retry_timing_guard);
        retry_result?;
        turn_context.turn_timing_state.record_sampling_retry();
    }
}

#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %step_context.turn.sub_id,
        model = %step_context.turn.model_info.slug,
        apps_enabled = step_context.turn.apps_enabled()
    )
)]
pub(crate) async fn built_tools(
    sess: &Session,
    step_context: &StepContext,
    cancellation_token: &CancellationToken,
) -> CodexResult<Arc<ToolRouter>> {
    let turn_context = step_context.turn.as_ref();
    let _router_build_timing_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::RouterBuild);
    let mcp_connection_manager = step_context.mcp.manager();
    let has_mcp_servers = mcp_connection_manager.has_servers();
    let all_mcp_tools = step_context
        .mcp_tools()
        .or_cancel(cancellation_token)
        .await?;
    let loaded_plugins = sess
        .services
        .plugins_manager
        .plugins_for_config(&turn_context.config.plugins_config_input())
        .instrument(trace_span!("built_tools.load_plugins"))
        .await;
    let connector_snapshot = step_context.mcp.config().connector_snapshot.clone();

    let apps_enabled = turn_context.apps_enabled();
    let accessible_connectors =
        apps_enabled.then(|| connectors::accessible_connectors_from_mcp_tools(all_mcp_tools));
    let accessible_connectors_with_enabled_state =
        accessible_connectors.as_ref().map(|connectors| {
            connectors::with_app_enabled_state(connectors.clone(), &turn_context.config)
        });
    let connectors = if apps_enabled {
        let connectors = codex_connectors::merge::merge_plugin_connectors_with_accessible(
            connector_snapshot
                .connector_ids()
                .iter()
                .map(|connector_id| connector_id.0.clone()),
            accessible_connectors.clone().unwrap_or_default(),
        );
        Some(connectors::with_app_enabled_state(
            connectors,
            &turn_context.config,
        ))
    } else {
        None
    };
    let tool_suggest_is_enabled = tool_suggest_enabled(turn_context);
    let auth = if tool_suggest_is_enabled {
        sess.services.auth_manager.auth().await
    } else {
        None
    };
    let endpoint_recommended_plugin_candidates = if tool_suggest_is_enabled {
        let plugins_config = turn_context.config.plugins_config_input();
        sess.services
            .plugins_manager
            .recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
                plugins_config: &plugins_config,
                loaded_plugins: &loaded_plugins,
                auth: auth.as_ref(),
                disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
                app_server_client_name: turn_context.app_server_client_name.as_deref(),
            })
            .await
    } else {
        None
    };
    let tool_suggest_candidates =
        if let Some(recommended_plugin_candidates) = endpoint_recommended_plugin_candidates {
            Some(ToolSuggestCandidates {
                tools: recommended_plugin_candidates,
                presentation: ToolSuggestPresentation::RecommendationContext,
            })
        } else {
            let loaded_plugin_app_connector_ids = connector_snapshot
                .connector_ids()
                .iter()
                .map(|connector_id| connector_id.0.clone())
                .collect::<Vec<_>>();
            async {
                if apps_enabled && tool_suggest_is_enabled {
                    if let Some(accessible_connectors) =
                        accessible_connectors_with_enabled_state.as_ref()
                    {
                        match connectors::list_tool_suggest_discoverable_tools_with_auth(
                            &turn_context.config,
                            sess.services.plugins_manager.as_ref(),
                            auth.as_ref(),
                            accessible_connectors.as_slice(),
                            &loaded_plugin_app_connector_ids,
                        )
                        .await
                        .map(|discoverable_tools| {
                            filter_request_plugin_install_discoverable_tools_for_client(
                                discoverable_tools,
                                turn_context.app_server_client_name.as_deref(),
                            )
                        }) {
                            Ok(discoverable_tools) if discoverable_tools.is_empty() => None,
                            Ok(discoverable_tools) => Some(ToolSuggestCandidates {
                                tools: discoverable_tools,
                                presentation: ToolSuggestPresentation::ListTool,
                            }),
                            Err(err) => {
                                warn!("failed to load discoverable tool suggestions: {err:#}");
                                None
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            .instrument(trace_span!("built_tools.load_discoverable_tools"))
            .await
        };
    let mcp_tool_exposure = build_mcp_tool_exposure(
        all_mcp_tools,
        connectors.as_deref(),
        &turn_context.config,
        search_tool_enabled(turn_context),
    );
    let mcp_tools = has_mcp_servers.then_some(mcp_tool_exposure.direct_tools);
    let deferred_mcp_tools = mcp_tool_exposure.deferred_tools;
    Ok(Arc::new(ToolRouter::from_context(
        step_context,
        ToolRouterParams {
            mcp_tools,
            deferred_mcp_tools,
            tool_suggest_candidates,
            extension_tool_executors: extension_tool_executors(sess),
            dynamic_tools: turn_context.dynamic_tools.as_slice(),
        },
        &sess.services.tool_search_handler_cache,
    )))
}

struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
    pending_managed_final: Option<PendingManagedFinal>,
    models_refresh_task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct PendingManagedFinal {
    item_id: String,
    agent_item: TurnItem,
    plan_item: Option<TurnItem>,
    facts: FinalizedTurnItemFacts,
}

fn precommit_hook_message(
    last_agent_message: Option<String>,
    provisional_final_pending: bool,
) -> Option<String> {
    if provisional_final_pending {
        None
    } else {
        last_agent_message
    }
}

enum PendingManagedFinalOutcome {
    Emitted(Option<String>),
    PendingInput,
    Rejected,
}

async fn await_models_refresh_task(task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        let _ = task.await;
    }
}

async fn abort_pending_managed_final_reservation(
    sess: &Session,
    turn_context: &TurnContext,
    pending: &mut Option<PendingManagedFinal>,
) -> CodexResult<()> {
    let Some(pending) = pending.take() else {
        return Ok(());
    };
    sess.services
        .task_evidence
        .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
        .await
        .map_err(CodexErr::InvalidRequest)
}

async fn abort_sampling_result_managed_final(
    sess: &Session,
    turn_context: &TurnContext,
    outcome: &mut CodexResult<SamplingRequestResult>,
) -> CodexResult<()> {
    let Ok(result) = outcome else {
        return Ok(());
    };
    abort_pending_managed_final_reservation(sess, turn_context, &mut result.pending_managed_final)
        .await
}

async fn commit_and_emit_pending_managed_final(
    sess: &Session,
    turn_context: &TurnContext,
    pending: PendingManagedFinal,
) -> CodexResult<PendingManagedFinalOutcome> {
    let mut completed_items = Vec::new();
    if let Some(plan_item) = pending.plan_item {
        completed_items.push(plan_item);
    }
    match pending.agent_item {
        TurnItem::AgentMessage(agent_message)
            if !agent_message_text(&agent_message).trim().is_empty() =>
        {
            completed_items.push(TurnItem::AgentMessage(agent_message));
        }
        TurnItem::AgentMessage(_) => {}
        _ => {
            sess.services
                .task_evidence
                .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            return Err(CodexErr::InvalidRequest(
                "managed final candidate is not an agent message".to_string(),
            ));
        }
    }
    if completed_items.is_empty() {
        sess.services
            .task_evidence
            .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
            .await
            .map_err(CodexErr::InvalidRequest)?;
        return Err(CodexErr::InvalidRequest(
            "managed final has no lifecycle item to complete".to_string(),
        ));
    }

    let emission_key = match sess
        .services
        .task_evidence
        .stage_final_emission_items(&turn_context.sub_id, &pending.item_id, &completed_items)
        .await
    {
        Ok(emission_key) => emission_key,
        Err(err) => {
            sess.services
                .task_evidence
                .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            return Err(CodexErr::InvalidRequest(err));
        }
    };
    let commit_boundary = sess
        .input_queue
        .commit_final_if_no_pending_input(&sess.active_turn, &turn_context.sub_id, || {
            sess.services
                .task_evidence
                .commit_final_item(&turn_context.sub_id, &pending.item_id)
        })
        .await;
    match commit_boundary {
        Ok(FinalCommitBoundary::Committed) => {}
        Ok(FinalCommitBoundary::PendingInput) => {
            sess.services
                .task_evidence
                .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            return Ok(PendingManagedFinalOutcome::PendingInput);
        }
        Ok(FinalCommitBoundary::Rejected) => {
            sess.services
                .task_evidence
                .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            return Ok(PendingManagedFinalOutcome::Rejected);
        }
        Err(err) => {
            sess.services
                .task_evidence
                .abort_final_reservation(&turn_context.sub_id, &pending.item_id)
                .await
                .map_err(CodexErr::InvalidRequest)?;
            return Err(CodexErr::InvalidRequest(err));
        }
    }

    if let Err(emit_error) = sess
        .emit_managed_final_items_checked(turn_context, completed_items)
        .await
    {
        return Err(CodexErr::InvalidRequest(emit_error));
    }
    sess.services
        .task_evidence
        .mark_final_emission_items_emitted(&turn_context.sub_id, &pending.item_id, &emission_key)
        .await
        .map_err(CodexErr::InvalidRequest)?;
    Ok(PendingManagedFinalOutcome::Emitted(
        pending.facts.last_agent_message,
    ))
}

pub(crate) async fn recover_pending_managed_final_outbox(sess: &Session) -> CodexResult<()> {
    let Some(emission) = sess
        .services
        .task_evidence
        .recoverable_final_emission()
        .await
        .map_err(CodexErr::InvalidRequest)?
    else {
        return Ok(());
    };
    let recovery_context = sess
        .new_default_turn_with_sub_id(emission.turn_id.clone())
        .await;
    if recovery_context.config.ephemeral {
        if sess
            .services
            .task_evidence
            .managed_final_state_for_turn(&emission.turn_id)
            .await
            == Some(ManagedFinalState::ItemsPending)
        {
            sess.emit_managed_final_items_checked(&recovery_context, emission.items.clone())
                .await
                .map_err(|err| {
                    CodexErr::InvalidRequest(format!(
                        "failed to emit committed in-memory final during recovery: {err}"
                    ))
                })?;
            sess.services
                .task_evidence
                .mark_final_emission_items_emitted(
                    &emission.turn_id,
                    &emission.item_id,
                    &emission.emission_key,
                )
                .await
                .map_err(CodexErr::InvalidRequest)?;
        }
        return emit_managed_final_terminal_checked(
            sess,
            &recovery_context,
            &emission.terminal_event,
        )
        .await
        .map_err(CodexErr::InvalidRequest);
    }
    let live_thread = sess
        .live_thread_for_persistence("recovering committed final outbox")
        .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;
    let history = live_thread
        .load_history(/*include_archived*/ true)
        .await
        .map_err(|err| {
            CodexErr::InvalidRequest(format!(
                "failed to inspect rollout history for committed final recovery: {err:#}"
            ))
        })?;
    let items_present = durable_managed_final_batch_present(
        &history.items,
        sess.thread_id,
        &emission.turn_id,
        &emission.item_id,
        &emission.items,
        recovery_context.history_mode,
        sess.show_raw_agent_reasoning(),
    );
    let existing_terminal = history.items.iter().find_map(|item| {
        let RolloutItem::EventMsg(actual @ EventMsg::TurnComplete(actual_complete)) = item else {
            return None;
        };
        let expected = match &emission.terminal_event {
            EventMsg::TurnComplete(expected) => expected,
            _ => return None,
        };
        if actual_complete.turn_id != expected.turn_id {
            return None;
        }
        if emission.terminal_event_staged {
            if !durable_final_event_matches(&emission.terminal_event, item) {
                return None;
            }
        } else if actual_complete.last_agent_message != expected.last_agent_message {
            return None;
        }
        Some(actual.clone())
    });
    if existing_terminal.is_some() && !items_present {
        return Err(CodexErr::InvalidRequest(
            "committed final rollout contains its terminal event without the exact item batch"
                .to_string(),
        ));
    }
    if !items_present {
        sess.emit_managed_final_items_checked(&recovery_context, emission.items.clone())
            .await
            .map_err(|err| {
                CodexErr::InvalidRequest(format!(
                    "failed to append committed final during recovery: {err}"
                ))
            })?;
    }
    sess.services
        .task_evidence
        .mark_final_emission_items_emitted(
            &emission.turn_id,
            &emission.item_id,
            &emission.emission_key,
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
    let proposed_terminal = existing_terminal.unwrap_or(emission.terminal_event);
    emit_managed_final_terminal_checked(sess, &recovery_context, &proposed_terminal)
        .await
        .map_err(CodexErr::InvalidRequest)
}

pub(crate) async fn emit_managed_final_terminal_checked(
    sess: &Session,
    turn_context: &TurnContext,
    proposed_event: &EventMsg,
) -> Result<(), String> {
    let staged_terminal = sess
        .services
        .task_evidence
        .stage_final_terminal_event(&turn_context.sub_id, proposed_event)
        .await?;
    if turn_context.config.ephemeral {
        sess.send_event_checked(turn_context, staged_terminal.clone())
            .await
            .map_err(|err| format!("failed to emit committed in-memory terminal event: {err}"))?;
        return sess
            .services
            .task_evidence
            .mark_final_terminal_completed(&turn_context.sub_id, &staged_terminal)
            .await;
    }
    let live_thread = sess
        .live_thread_for_persistence("reconciling managed final terminal outbox")
        .map_err(|err| err.to_string())?;
    let history = live_thread
        .load_history(/*include_archived*/ true)
        .await
        .map_err(|err| {
            format!("failed to inspect rollout history for managed terminal recovery: {err:#}")
        })?;
    if !history
        .items
        .iter()
        .any(|item| durable_final_event_matches(&staged_terminal, item))
    {
        sess.send_event_checked(turn_context, staged_terminal.clone())
            .await
            .map_err(|err| format!("failed to append committed terminal event: {err}"))?;
    }
    sess.services
        .task_evidence
        .mark_final_terminal_completed(&turn_context.sub_id, &staged_terminal)
        .await
}

fn durable_managed_final_batch_present(
    history: &[RolloutItem],
    thread_id: codex_protocol::ThreadId,
    turn_id: &str,
    provisional_item_id: &str,
    items: &[TurnItem],
    history_mode: ThreadHistoryMode,
    show_raw_agent_reasoning: bool,
) -> bool {
    let history = if history_mode == ThreadHistoryMode::Legacy {
        let Some(provisional_index) = history.iter().rposition(|item| {
            matches!(
                item,
                RolloutItem::ResponseItem(response)
                    if response.id() == Some(provisional_item_id)
            )
        }) else {
            return false;
        };
        &history[provisional_index.saturating_add(1)..]
    } else {
        history
    };
    let mut lifecycle_events = items
        .iter()
        .cloned()
        .map(|item| {
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id,
                turn_id: turn_id.to_string(),
                item,
                started_at_ms: 0,
            })
        })
        .collect::<Vec<_>>();
    lifecycle_events.extend(items.iter().cloned().map(|item| {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: turn_id.to_string(),
            item,
            completed_at_ms: 0,
        })
    }));

    let mut expected = Vec::new();
    for event in lifecycle_events {
        let legacy_events = event.as_legacy_events(show_raw_agent_reasoning);
        for candidate in std::iter::once(event).chain(legacy_events) {
            let rollout_item = RolloutItem::EventMsg(candidate.clone());
            if codex_rollout::is_persisted_rollout_item(&rollout_item, history_mode) {
                expected.push(candidate);
            }
        }
    }
    !expected.is_empty()
        && history.windows(expected.len()).any(|window| {
            expected
                .iter()
                .zip(window)
                .all(|(expected, actual)| durable_final_event_matches(expected, actual))
        })
}

fn durable_final_event_matches(expected: &EventMsg, actual: &RolloutItem) -> bool {
    let RolloutItem::EventMsg(actual) = actual else {
        return false;
    };
    match (expected, actual) {
        (EventMsg::ItemStarted(expected), EventMsg::ItemStarted(actual)) => {
            expected.thread_id == actual.thread_id
                && expected.turn_id == actual.turn_id
                && serde_json::to_value(&expected.item).ok()
                    == serde_json::to_value(&actual.item).ok()
        }
        (EventMsg::ItemCompleted(expected), EventMsg::ItemCompleted(actual)) => {
            expected.thread_id == actual.thread_id
                && expected.turn_id == actual.turn_id
                && serde_json::to_value(&expected.item).ok()
                    == serde_json::to_value(&actual.item).ok()
        }
        _ => serde_json::to_value(expected).ok() == serde_json::to_value(actual).ok(),
    }
}

/// Ephemeral per-response state for streaming a single proposed plan.
/// This is intentionally not persisted or stored in session/state since it
/// only exists while a response is actively streaming. The final plan text
/// is extracted from the completed assistant message.
/// Tracks a single proposed plan item across a streaming response.
struct ProposedPlanItemState {
    item_id: String,
    started: bool,
    completed: bool,
    completion_durable: bool,
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
struct PlanModeStreamState {
    /// Agent message items started by the model but deferred until we see non-plan text.
    pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: HashSet<String>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: HashSet::new(),
            leading_whitespace_by_item: HashMap::new(),
            plan_item_state: ProposedPlanItemState::new(turn_id),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct AssistantMessageStreamParsers {
    plan_mode: bool,
    parsers_by_item: HashMap<String, AssistantTextStreamParser>,
}

type ParsedAssistantTextDelta = AssistantTextChunk;

impl AssistantMessageStreamParsers {
    pub(super) fn new(plan_mode: bool) -> Self {
        Self {
            plan_mode,
            parsers_by_item: HashMap::new(),
        }
    }

    fn parser_mut(&mut self, item_id: &str) -> &mut AssistantTextStreamParser {
        let plan_mode = self.plan_mode;
        self.parsers_by_item
            .entry(item_id.to_string())
            .or_insert_with(|| AssistantTextStreamParser::new(plan_mode))
    }

    pub(super) fn seed_item_text(&mut self, item_id: &str, text: &str) -> ParsedAssistantTextDelta {
        if text.is_empty() {
            return ParsedAssistantTextDelta::default();
        }
        self.parser_mut(item_id).push_str(text)
    }

    pub(super) fn parse_delta(&mut self, item_id: &str, delta: &str) -> ParsedAssistantTextDelta {
        self.parser_mut(item_id).push_str(delta)
    }

    pub(super) fn finish_item(&mut self, item_id: &str) -> ParsedAssistantTextDelta {
        let Some(mut parser) = self.parsers_by_item.remove(item_id) else {
            return ParsedAssistantTextDelta::default();
        };
        parser.finish()
    }

    fn drain_finished(&mut self) -> Vec<(String, ParsedAssistantTextDelta)> {
        let parsers_by_item = std::mem::take(&mut self.parsers_by_item);
        parsers_by_item
            .into_iter()
            .map(|(item_id, mut parser)| (item_id, parser.finish()))
            .collect()
    }
}

impl ProposedPlanItemState {
    fn new(turn_id: &str) -> Self {
        Self {
            item_id: format!("{turn_id}-plan"),
            started: false,
            completed: false,
            completion_durable: false,
        }
    }

    async fn start(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.started || self.completed {
            return;
        }
        self.started = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text: String::new(),
        });
        sess.emit_turn_item_started(turn_context, &item).await;
    }

    async fn push_delta(&mut self, sess: &Session, turn_context: &TurnContext, delta: &str) {
        if self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        let event = PlanDeltaEvent {
            thread_id: sess.thread_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
        require_durable_lifecycle: bool,
    ) -> CodexResult<bool> {
        if self.completed || !self.started {
            return Ok(self.completed && self.completion_durable);
        }
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        if require_durable_lifecycle {
            sess.emit_turn_item_completed_checked(turn_context, item)
                .await
                .map_err(CodexErr::InvalidRequest)?;
        } else {
            sess.emit_turn_item_completed(turn_context, item).await;
        }
        self.completed = true;
        self.completion_durable = require_durable_lifecycle;
        Ok(self.completion_durable)
    }
}

/// In plan mode we defer agent message starts until the parser emits non-plan
/// text. The parser buffers each line until it can rule out a tag prefix, so
/// plan-only outputs never show up as empty assistant messages.
async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string());
    }
}

/// Agent messages are text-only today; concatenate all text entries.
fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

pub(super) fn realtime_text_for_event(msg: &EventMsg) -> Option<(String, Option<MessagePhase>)> {
    match msg {
        EventMsg::AgentMessage(event) => Some((event.message.clone(), event.phase.clone())),
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some((agent_message_text(item), item.phase.clone())),
            _ => None,
        },
        EventMsg::Error(_)
        | EventMsg::Warning(_)
        | EventMsg::GuardianWarning(_)
        | EventMsg::RealtimeConversationStarted(_)
        | EventMsg::RealtimeConversationSdp(_)
        | EventMsg::RealtimeConversationRealtime(_)
        | EventMsg::RealtimeConversationClosed(_)
        | EventMsg::ModelReroute(_)
        | EventMsg::ModelVerification(_)
        | EventMsg::TurnModerationMetadata(_)
        | EventMsg::SafetyBuffering(_)
        | EventMsg::ContextCompacted(_)
        | EventMsg::ThreadRolledBack(_)
        | EventMsg::TurnStarted(_)
        | EventMsg::ThreadSettingsApplied(_)
        | EventMsg::TurnComplete(_)
        | EventMsg::TokenCount(_)
        | EventMsg::UserMessage(_)
        | EventMsg::AgentReasoning(_)
        | EventMsg::AgentReasoningRawContent(_)
        | EventMsg::AgentReasoningSectionBreak(_)
        | EventMsg::SessionConfigured(_)
        | EventMsg::ThreadGoalUpdated(_)
        | EventMsg::McpStartupUpdate(_)
        | EventMsg::McpStartupComplete(_)
        | EventMsg::McpToolCallBegin(_)
        | EventMsg::McpToolCallEnd(_)
        | EventMsg::WebSearchBegin(_)
        | EventMsg::WebSearchEnd(_)
        | EventMsg::ExecCommandBegin(_)
        | EventMsg::ExecCommandOutputDelta(_)
        | EventMsg::TerminalInteraction(_)
        | EventMsg::ExecCommandEnd(_)
        | EventMsg::PatchApplyBegin(_)
        | EventMsg::PatchApplyUpdated(_)
        | EventMsg::PatchApplyEnd(_)
        | EventMsg::ImageGenerationBegin(_)
        | EventMsg::ImageGenerationEnd(_)
        | EventMsg::ViewImageToolCall(_)
        | EventMsg::ExecApprovalRequest(_)
        | EventMsg::RequestPermissions(_)
        | EventMsg::RequestUserInput(_)
        | EventMsg::DynamicToolCallRequest(_)
        | EventMsg::DynamicToolCallResponse(_)
        | EventMsg::GuardianAssessment(_)
        | EventMsg::ElicitationRequest(_)
        | EventMsg::ApplyPatchApprovalRequest(_)
        | EventMsg::DeprecationNotice(_)
        | EventMsg::StreamError(_)
        | EventMsg::TurnDiff(_)
        | EventMsg::RealtimeConversationListVoicesResponse(_)
        | EventMsg::PlanUpdate(_)
        | EventMsg::TurnAborted(_)
        | EventMsg::ShutdownComplete
        | EventMsg::EnteredReviewMode(_)
        | EventMsg::ExitedReviewMode(_)
        | EventMsg::RawResponseItem(_)
        | EventMsg::ItemStarted(_)
        | EventMsg::HookStarted(_)
        | EventMsg::HookCompleted(_)
        | EventMsg::AgentMessageContentDelta(_)
        | EventMsg::PlanDelta(_)
        | EventMsg::ReasoningContentDelta(_)
        | EventMsg::ReasoningRawContentDelta(_)
        | EventMsg::CollabAgentSpawnBegin(_)
        | EventMsg::CollabAgentSpawnEnd(_)
        | EventMsg::CollabAgentInteractionBegin(_)
        | EventMsg::CollabAgentInteractionEnd(_)
        | EventMsg::CollabWaitingBegin(_)
        | EventMsg::CollabWaitingEnd(_)
        | EventMsg::CollabCloseBegin(_)
        | EventMsg::CollabCloseEnd(_)
        | EventMsg::CollabResumeBegin(_)
        | EventMsg::CollabResumeEnd(_)
        | EventMsg::SubAgentActivity(_) => None,
    }
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    for segment in segments {
        match segment {
            ProposedPlanSegment::Normal(delta) => {
                if delta.is_empty() {
                    continue;
                }
                let has_non_whitespace = delta.chars().any(|ch| !ch.is_whitespace());
                if !has_non_whitespace && !state.started_agent_message_items.contains(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains(item_id) {
                    if let Some(prefix) = state.leading_whitespace_by_item.remove(item_id) {
                        format!("{prefix}{delta}")
                    } else {
                        delta
                    }
                } else {
                    delta
                };
                maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;

                let event = AgentMessageContentDeltaEvent {
                    thread_id: sess.thread_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id: item_id.to_string(),
                    delta,
                };
                sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                    .await;
            }
            ProposedPlanSegment::ProposedPlanStart => {
                if !state.plan_item_state.completed {
                    state.plan_item_state.start(sess, turn_context).await;
                }
            }
            ProposedPlanSegment::ProposedPlanDelta(delta) => {
                if !state.plan_item_state.completed {
                    if !state.plan_item_state.started {
                        state.plan_item_state.start(sess, turn_context).await;
                    }
                    state
                        .plan_item_state
                        .push_delta(sess, turn_context, &delta)
                        .await;
                }
            }
            ProposedPlanSegment::ProposedPlanEnd => {}
        }
    }
}

async fn emit_streamed_assistant_text_delta(
    sess: &Session,
    turn_context: &TurnContext,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    item_id: &str,
    parsed: ParsedAssistantTextDelta,
) {
    if parsed.is_empty() {
        return;
    }
    if !parsed.citations.is_empty() {
        // Citation extraction is intentionally local for now; we strip citations from display text
        // but do not yet surface them in protocol events.
        let _citations = parsed.citations;
    }
    if let Some(state) = plan_mode_state {
        if !parsed.plan_segments.is_empty() {
            handle_plan_segments(sess, turn_context, state, item_id, parsed.plan_segments).await;
        }
        return;
    }
    if parsed.visible_text.is_empty() {
        return;
    }
    let event = AgentMessageContentDeltaEvent {
        thread_id: sess.thread_id.to_string(),
        turn_id: turn_context.sub_id.clone(),
        item_id: item_id.to_string(),
        delta: parsed.visible_text,
    };
    sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
        .await;
}

/// Flush buffered assistant text parser state when an assistant message item ends.
async fn flush_assistant_text_segments_for_item(
    sess: &Session,
    turn_context: &TurnContext,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
    item_id: &str,
) {
    let parsed = parsers.finish_item(item_id);
    emit_streamed_assistant_text_delta(sess, turn_context, plan_mode_state, item_id, parsed).await;
}

/// Flush any remaining buffered assistant text parser state at response completion.
async fn flush_assistant_text_segments_all(
    sess: &Session,
    turn_context: &TurnContext,
    mut plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
) {
    for (item_id, parsed) in parsers.drain_finished() {
        emit_streamed_assistant_text_delta(
            sess,
            turn_context,
            plan_mode_state.as_deref_mut(),
            &item_id,
            parsed,
        )
        .await;
    }
}

/// Emit completion for plan items by parsing the finalized assistant message.
async fn maybe_complete_plan_item_from_message(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item: &ResponseItem,
    require_durable_lifecycle: bool,
) -> CodexResult<Option<String>> {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let mut text = String::new();
        for entry in content {
            if let ContentItem::OutputText { text: chunk } = entry {
                text.push_str(chunk);
            }
        }
        if let Some(plan_text) = extract_proposed_plan_text(&text) {
            let (plan_text, _citations) = strip_citations(&plan_text);
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text, require_durable_lifecycle)
                .await?;
            if state.plan_item_state.completed
                && (!require_durable_lifecycle || state.plan_item_state.completion_durable)
            {
                return Ok(Some(state.plan_item_state.item_id.clone()));
            }
        }
    }
    Ok(None)
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
    require_durable_lifecycle: bool,
) -> CodexResult<Option<String>> {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        state.started_agent_message_items.remove(&agent_message_id);
        return Ok(None);
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                    phase: None,
                    memory_citation: None,
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone());
    }

    if require_durable_lifecycle {
        sess.emit_turn_item_completed_checked(turn_context, TurnItem::AgentMessage(agent_message))
            .await
            .map_err(CodexErr::InvalidRequest)?;
    } else {
        sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
            .await;
    }
    state.started_agent_message_items.remove(&agent_message_id);
    Ok(Some(agent_message_id))
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
    require_durable_lifecycle: bool,
) -> CodexResult<Option<String>> {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(
                sess,
                turn_context,
                agent_message,
                state,
                require_durable_lifecycle,
            )
            .await
        }
        _ => {
            let item_id = turn_item.id();
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            if require_durable_lifecycle {
                sess.emit_turn_item_completed_checked(turn_context, turn_item)
                    .await
                    .map_err(CodexErr::InvalidRequest)?;
            } else {
                sess.emit_turn_item_completed(turn_context, turn_item).await;
            }
            Ok(Some(item_id))
        }
    }
}

struct PlanModeAssistantDone {
    completed_item_id: Option<String>,
    last_agent_message: Option<String>,
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_store: &codex_extension_api::ExtensionData,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    require_durable_lifecycle: bool,
    prefinalized_turn_item: Option<FinalizedTurnItem>,
) -> CodexResult<Option<PlanModeAssistantDone>> {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        let mut finalized_facts = None;
        let finalized_turn_item = match prefinalized_turn_item {
            Some(finalized_turn_item) => Some(finalized_turn_item),
            None => {
                finalize_non_tool_response_item(
                    sess,
                    TurnItemContributorPolicy::Run(turn_store),
                    item,
                    /*plan_mode*/ true,
                )
                .await
            }
        };
        if let Some(finalized_turn_item) = finalized_turn_item.as_ref() {
            if require_durable_lifecycle
                && !matches!(
                    &finalized_turn_item.turn_item,
                    TurnItem::AgentMessage(agent_message)
                        if Some(agent_message.id.as_str()) == item.id()
                            && !matches!(
                                agent_message.phase,
                                Some(MessagePhase::Commentary)
                            )
                )
            {
                return Err(CodexErr::InvalidRequest(
                    "final turn-item contributors changed the committed item identity or finality"
                        .to_string(),
                ));
            }
            finalized_facts = Some(finalized_turn_item.facts.clone());
        }
        record_completed_response_item_with_finalized_facts(
            sess,
            turn_context,
            item,
            finalized_facts.as_ref(),
            /*suppress_external_effects*/ false,
            require_durable_lifecycle,
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
        if require_durable_lifecycle {
            let item_id = item.id().ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "committed final response is missing its item identity".to_string(),
                )
            })?;
            sess.services
                .task_evidence
                .mark_final_item_persisted(&turn_context.sub_id, item_id, item)
                .await
                .map_err(CodexErr::InvalidRequest)?;
        }

        let _completed_plan_item_id = maybe_complete_plan_item_from_message(
            sess,
            turn_context,
            state,
            item,
            require_durable_lifecycle,
        )
        .await?;

        let mut completed_item_id = None;
        if let Some(finalized_turn_item) = finalized_turn_item {
            completed_item_id = emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                finalized_turn_item.turn_item,
                previously_active_item,
                state,
                require_durable_lifecycle,
            )
            .await?;
        }
        let final_last_agent_message = finalized_facts
            .as_ref()
            .and_then(|facts| facts.last_agent_message.clone());
        return Ok(Some(PlanModeAssistantDone {
            completed_item_id,
            last_agent_message: final_last_agent_message,
        }));
    }
    Ok(None)
}

#[instrument(level = "trace", skip_all)]
async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    let mut first_error = None;
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                let response_item = response_input.into();
                sess.record_conversation_items(&turn_context, std::slice::from_ref(&response_item))
                    .await;
                mark_thread_memory_mode_polluted_if_external_context(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &response_item,
                )
                .await;
            }
            Err(err) => {
                error!("in-flight tool future failed during drain: {err}");
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn assign_missing_streamed_response_item_id(
    item: &mut ResponseItem,
    active_item: Option<&TurnItem>,
) {
    if item.id().is_some() {
        return;
    }

    let active_item_id = active_item
        .map(TurnItem::id)
        .filter(|item_id| !item_id.is_empty());
    item.set_id(active_item_id);
    Session::assign_missing_response_item_id(item);
}

fn is_final_assistant_response_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message { role, phase, .. }
            if role == "assistant" && !matches!(phase, Some(MessagePhase::Commentary))
    )
}

fn response_item_matches_active_final(item: &ResponseItem, active_item: Option<&TurnItem>) -> bool {
    let Some(TurnItem::AgentMessage(active_message)) = active_item else {
        return false;
    };
    matches!(
        item,
        ResponseItem::Message {
            id: Some(item_id),
            role,
            ..
        } if role == "assistant" && item_id == &active_message.id
    )
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug
    )
)]
async fn try_run_sampling_request(
    tool_runtime: ToolCallRuntime,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_store: Arc<codex_extension_api::ExtensionData>,
    client_session: &mut ModelClientSession,
    responses_metadata: &CodexResponsesMetadata,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    preparation_timing_guard: &mut Option<TurnTimingGuard>,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    sess.reserve_rollout_model_call(turn_context.as_ref())?;
    feedback_tags!(
        model = turn_context.model_info.slug.clone(),
        approval_policy = turn_context.approval_policy.value(),
        sandbox_policy = &turn_context.sandbox_policy(),
        effort = turn_context.reasoning_effort,
        auth_mode = sess.services.auth_manager.auth_mode(),
        features = sess.features.enabled_features(),
    );
    let inference_trace = sess.services.rollout_thread_trace.inference_trace_context(
        turn_context.sub_id.as_str(),
        turn_context.model_info.slug.as_str(),
        turn_context.provider.info().name.as_str(),
    );
    let sampling_timing_guard = turn_context.turn_timing_state.begin_sampling();
    let uses_sequential_cutoff_reasoning_summaries = turn_context
        .config
        .features
        .enabled(Feature::ConcurrentReasoningSummaries)
        && turn_context.provider.info().is_openai();
    let model_request_timing_guard = turn_context.turn_timing_state.begin_model_request_wait();
    drop(preparation_timing_guard.take());
    let startup_snapshot = sess.startup_timing.complete_snapshot();
    trace!(
        startup_timing_schema_version = startup_snapshot.schema_version,
        startup_timing_correlation_id = %startup_snapshot.correlation_id,
        startup_timing_duration_ns = startup_snapshot.inclusive_duration_ns,
        startup_timing_profile_valid = startup_snapshot.profile_valid,
        startup_prewarm_status = ?startup_snapshot.prewarm_status,
        startup_transport_preconnect_ns = startup_snapshot.phases.transport_preconnect_ns,
        startup_prewarm_preparation_ns = startup_snapshot.phases.prewarm_preparation_ns,
        startup_prewarm_request_ns = startup_snapshot.phases.prewarm_request_ns,
        startup_first_turn_wait_ns = startup_snapshot.phases.first_turn_prewarm_wait_ns,
        startup_executor_readiness_ns = startup_snapshot.phases.executor_readiness_ns,
        "startup timing snapshot frozen at first model send"
    );
    client_session.set_turn_timing(Arc::clone(&turn_context.turn_timing_state));
    let stream_result = client_session
        .stream(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            turn_context.reasoning_effort.clone(),
            turn_context.reasoning_summary,
            turn_context.config.service_tier.clone(),
            responses_metadata,
            &inference_trace,
        )
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await;
    drop(model_request_timing_guard);
    let mut stream = stream_result??;
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut active_tool_argument_diff_consumer: Option<(
        String,
        Box<dyn ToolArgumentDiffConsumer>,
    )> = None;
    let mut should_emit_turn_diff = false;
    let mut should_emit_token_count = false;
    let mut latest_models_etag = None;
    let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
    let plan_mode = turn_context.collaboration_mode.mode == ModeKind::Plan;
    let mut assistant_message_stream_parsers = AssistantMessageStreamParsers::new(plan_mode);
    let mut plan_mode_state = plan_mode.then(|| PlanModeStreamState::new(&turn_context.sub_id));
    let managed_task_turn = sess
        .services
        .task_evidence
        .manages_turn(&turn_context.sub_id)
        .await;
    let defer_streamed_turn_items_for_contributors =
        managed_task_turn || !sess.services.extensions.turn_item_contributors().is_empty();
    let mut active_item_is_streaming_to_client = false;
    let mut active_item_is_provisional_final = false;
    let mut active_item_final_committed = false;
    let mut active_final_reservation: Option<String> = None;
    let mut pending_managed_final: Option<PendingManagedFinal> = None;
    let receiving_span = trace_span!("receiving_stream");
    let mut outcome: CodexResult<SamplingRequestResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
            codex.request.reasoning_effort = %reasoning_effort,
            gen_ai.usage.input_tokens = field::Empty,
            gen_ai.usage.cache_read.input_tokens = field::Empty,
            gen_ai.usage.output_tokens = field::Empty,
            codex.usage.reasoning_output_tokens = field::Empty,
            codex.usage.total_tokens = field::Empty,
        );

        let model_stream_wait_timing_guard =
            turn_context.turn_timing_state.begin_model_stream_wait();
        let stream_event = stream
            .next()
            .instrument(trace_span!(parent: &handle_responses, "receiving"))
            .or_cancel(&cancellation_token)
            .await;
        drop(model_stream_wait_timing_guard);
        let event = match stream_event {
            Ok(event) => event,
            Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
        };

        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(err)) => break Err(err),
            None => {
                break Err(CodexErr::Stream(
                    "stream closed before response.completed".into(),
                    None,
                ));
            }
        };

        let _model_stream_processing_timing_guard = turn_context
            .turn_timing_state
            .begin_model_stream_processing();
        sess.services
            .session_telemetry
            .record_responses(&handle_responses, &event);
        record_turn_ttft_metric(&turn_context, &event).await;
        if pending_managed_final.is_some()
            && !matches!(
                &event,
                ResponseEvent::Completed { .. }
                    | ResponseEvent::ModelsEtag(_)
                    | ResponseEvent::ModelVerifications(_)
                    | ResponseEvent::RateLimits(_)
                    | ResponseEvent::SafetyBuffering(_)
                    | ResponseEvent::ServerModel(_)
                    | ResponseEvent::ServerReasoningIncluded(_)
                    | ResponseEvent::TurnModerationMetadata(_)
            )
        {
            break Err(CodexErr::InvalidRequest(
                "model emitted output after its managed final response completed".to_string(),
            ));
        }

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(mut item) => {
                let raw_item_is_final = is_final_assistant_response_item(&item);
                if turn_context.item_ids_enabled() || managed_task_turn || raw_item_is_final {
                    assign_missing_streamed_response_item_id(&mut item, active_item.as_ref());
                }
                if let Some((_, mut consumer)) = active_tool_argument_diff_consumer.take()
                    && let Ok(Some(event)) = consumer.finish()
                {
                    sess.send_event(&turn_context, event).await;
                }
                let previously_active_item = active_item.take();
                let mut suppress_external_effects =
                    std::mem::take(&mut active_item_is_provisional_final);
                let mut final_committed = std::mem::take(&mut active_item_final_committed);
                let deferred_final_reservation = active_final_reservation.take();
                let mut prefinalized_managed_item = if managed_task_turn
                    && matches!(ToolRouter::build_tool_call(item.clone()), Ok(None))
                {
                    finalize_non_tool_response_item(
                        sess.as_ref(),
                        TurnItemContributorPolicy::Run(turn_store.as_ref()),
                        &item,
                        plan_mode,
                    )
                    .await
                } else {
                    None
                };
                let contributed_item_is_final =
                    prefinalized_managed_item.as_ref().is_some_and(|finalized| {
                        matches!(
                            &finalized.turn_item,
                            TurnItem::AgentMessage(agent_message)
                                if !matches!(
                                    agent_message.phase,
                                    Some(MessagePhase::Commentary)
                                )
                        )
                    });
                let item_is_final = raw_item_is_final || contributed_item_is_final;
                let item_matches_active_final =
                    response_item_matches_active_final(&item, previously_active_item.as_ref());
                let active_was_final_candidate = suppress_external_effects
                    || final_committed
                    || deferred_final_reservation.is_some();
                let active_was_streamed = active_item_is_streaming_to_client;
                let mut completed_item_id = item.id().map(str::to_owned);

                if managed_task_turn && item_is_final {
                    if final_committed || deferred_final_reservation.is_some() {
                        break Err(CodexErr::InvalidRequest(
                            "managed final response was committed before post-response acceptance"
                                .to_string(),
                        ));
                    }
                    if active_was_streamed {
                        break Err(CodexErr::InvalidRequest(
                            "managed final response was exposed before post-response acceptance"
                                .to_string(),
                        ));
                    }
                    if pending_managed_final.is_some() {
                        break Err(CodexErr::InvalidRequest(
                            "model emitted more than one managed final response".to_string(),
                        ));
                    }
                    if let Err(err) = drain_in_flight(
                        &mut in_flight,
                        Arc::clone(&sess),
                        Arc::clone(&turn_context),
                    )
                    .await
                    {
                        break Err(err);
                    }
                    let Some(finalized_turn_item) = prefinalized_managed_item.take() else {
                        break Err(CodexErr::InvalidRequest(
                            "managed final response could not be converted into a final turn item"
                                .to_string(),
                        ));
                    };
                    let item_id = item.id().expect("managed final item id").to_string();
                    match &finalized_turn_item.turn_item {
                        TurnItem::AgentMessage(agent_message)
                            if agent_message.id == item_id
                                && !matches!(
                                    agent_message.phase,
                                    Some(MessagePhase::Commentary)
                                ) => {}
                        _ => {
                            break Err(CodexErr::InvalidRequest(
                                "final turn-item contributors changed the managed final identity or finality"
                                    .to_string(),
                            ));
                        }
                    }
                    let reserved = sess
                        .services
                        .task_evidence
                        .authorize_final_item(&turn_context.sub_id, &item_id)
                        .await
                        .map_err(CodexErr::InvalidRequest)?;
                    let persisted = record_completed_response_item_with_finalized_facts(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &item,
                        Some(&finalized_turn_item.facts),
                        /*suppress_external_effects*/ true,
                        /*require_durable_persistence*/ false,
                    )
                    .await
                    .map_err(CodexErr::InvalidRequest)?;
                    if !persisted {
                        sess.services
                            .task_evidence
                            .abort_final_reservation(&turn_context.sub_id, &item_id)
                            .await
                            .map_err(CodexErr::InvalidRequest)?;
                        break Err(CodexErr::InvalidRequest(
                            "managed final response could not be durably buffered".to_string(),
                        ));
                    }
                    sess.services
                        .task_evidence
                        .mark_final_item_persisted(&turn_context.sub_id, &item_id, &item)
                        .await
                        .map_err(CodexErr::InvalidRequest)?;
                    if reserved {
                        let plan_item = if plan_mode {
                            raw_assistant_output_text_from_item(&item)
                                .and_then(|text| extract_proposed_plan_text(&text))
                                .map(|text| {
                                    let (text, _) = strip_citations(&text);
                                    TurnItem::Plan(PlanItem {
                                        id: format!("{}-plan", turn_context.sub_id),
                                        text,
                                    })
                                })
                        } else {
                            None
                        };
                        last_agent_message = finalized_turn_item.facts.last_agent_message.clone();
                        pending_managed_final = Some(PendingManagedFinal {
                            item_id,
                            agent_item: finalized_turn_item.turn_item,
                            plan_item,
                            facts: finalized_turn_item.facts,
                        });
                    } else {
                        last_agent_message = None;
                    }
                    continue;
                }

                if final_committed && (!item_is_final || !item_matches_active_final) {
                    break Err(CodexErr::InvalidRequest(
                        "committed final output did not match its completed stream item"
                            .to_string(),
                    ));
                }

                let needs_late_final_authorization = item_is_final
                    && !final_committed
                    && (!active_was_final_candidate || !item_matches_active_final);
                if needs_late_final_authorization {
                    if active_was_streamed {
                        break Err(CodexErr::InvalidRequest(
                            "assistant item changed identity or finality after public streaming"
                                .to_string(),
                        ));
                    }
                    if let Some(reserved_item_id) = deferred_final_reservation.as_deref() {
                        sess.services
                            .task_evidence
                            .abort_final_reservation(&turn_context.sub_id, reserved_item_id)
                            .await
                            .map_err(CodexErr::InvalidRequest)?;
                    }
                    if let Err(err) = drain_in_flight(
                        &mut in_flight,
                        Arc::clone(&sess),
                        Arc::clone(&turn_context),
                    )
                    .await
                    {
                        break Err(err);
                    }
                    let item_id = item
                        .id()
                        .map(str::to_owned)
                        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                    item.set_id(Some(item_id.clone()));
                    let reserved = sess
                        .services
                        .task_evidence
                        .authorize_final_item(&turn_context.sub_id, &item_id)
                        .await
                        .unwrap_or(false);
                    final_committed = reserved
                        && sess
                            .services
                            .task_evidence
                            .commit_final_item(&turn_context.sub_id, &item_id)
                            .await
                            .unwrap_or(false);
                    suppress_external_effects = !final_committed;
                    completed_item_id = Some(item_id);
                } else if let Some(item_id) = deferred_final_reservation {
                    if !item_is_final || !item_matches_active_final {
                        sess.services
                            .task_evidence
                            .abort_final_reservation(&turn_context.sub_id, &item_id)
                            .await
                            .map_err(CodexErr::InvalidRequest)?;
                        suppress_external_effects = false;
                        final_committed = false;
                    } else {
                        final_committed = sess
                            .services
                            .task_evidence
                            .commit_final_item(&turn_context.sub_id, &item_id)
                            .await
                            .unwrap_or(false);
                        if !final_committed {
                            suppress_external_effects = true;
                        }
                    }
                } else if active_was_final_candidate && !item_is_final {
                    suppress_external_effects = false;
                }
                let completed_final_matches = final_committed
                    && item_is_final
                    && completed_item_id.as_deref() == item.id()
                    && (needs_late_final_authorization || item_matches_active_final);
                let require_durable_lifecycle = completed_final_matches && managed_task_turn;
                let provisional_response_item = suppress_external_effects.then(|| item.clone());
                let previously_streamed_item = if active_item_is_streaming_to_client {
                    previously_active_item
                } else {
                    None
                };
                active_item_is_streaming_to_client = false;
                if let Some(previous) = previously_streamed_item.as_ref()
                    && matches!(previous, TurnItem::AgentMessage(_))
                {
                    let item_id = previous.id();
                    flush_assistant_text_segments_for_item(
                        &sess,
                        &turn_context,
                        plan_mode_state.as_mut(),
                        &mut assistant_message_stream_parsers,
                        &item_id,
                    )
                    .await;
                }
                if !suppress_external_effects && let Some(state) = plan_mode_state.as_mut() {
                    let prefinalized_plan_item =
                        matches!(&item, ResponseItem::Message { role, .. } if role == "assistant")
                            .then(|| prefinalized_managed_item.take())
                            .flatten();
                    let plan_mode_done = match handle_assistant_item_done_in_plan_mode(
                        &sess,
                        &turn_context,
                        turn_store.as_ref(),
                        &item,
                        state,
                        previously_streamed_item.as_ref(),
                        require_durable_lifecycle,
                        prefinalized_plan_item,
                    )
                    .await
                    {
                        Ok(done) => done,
                        Err(err) => break Err(err),
                    };
                    if let Some(plan_mode_done) = plan_mode_done {
                        let completed_item_matches = completed_final_matches
                            && plan_mode_done.completed_item_id.as_deref()
                                == completed_item_id.as_deref();
                        if require_durable_lifecycle
                            && completed_item_matches
                            && let Some(item_id) = completed_item_id.as_deref()
                            && let Err(err) = sess
                                .services
                                .task_evidence
                                .mark_final_item_completed(&turn_context.sub_id, item_id)
                                .await
                        {
                            break Err(CodexErr::InvalidRequest(err));
                        }
                        if completed_item_matches {
                            last_agent_message = plan_mode_done.last_agent_message;
                        }
                        continue;
                    }
                }

                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    turn_store: Arc::clone(&turn_store),
                    tool_runtime: tool_runtime.clone(),
                    cancellation_token: cancellation_token.child_token(),
                };

                let preempt_for_mailbox_mail = match &item {
                    ResponseItem::Message { role, phase, .. } => {
                        role == "assistant" && matches!(phase, Some(MessagePhase::Commentary))
                    }
                    ResponseItem::Reasoning { .. } => true,
                    ResponseItem::AgentMessage { .. } => false,
                    ResponseItem::AdditionalTools { .. }
                    | ResponseItem::LocalShellCall { .. }
                    | ResponseItem::FunctionCall { .. }
                    | ResponseItem::ToolSearchCall { .. }
                    | ResponseItem::FunctionCallOutput { .. }
                    | ResponseItem::CustomToolCall { .. }
                    | ResponseItem::CustomToolCallOutput { .. }
                    | ResponseItem::ToolSearchOutput { .. }
                    | ResponseItem::WebSearchCall { .. }
                    | ResponseItem::ImageGenerationCall { .. }
                    | ResponseItem::Compaction { .. }
                    | ResponseItem::CompactionTrigger { .. }
                    | ResponseItem::ContextCompaction { .. }
                    | ResponseItem::Other => false,
                };

                let output_result = match handle_output_item_done(
                    &mut ctx,
                    item,
                    previously_streamed_item,
                    suppress_external_effects,
                    require_durable_lifecycle,
                    prefinalized_managed_item,
                )
                .instrument(handle_responses)
                .await
                {
                    Ok(output_result) => output_result,
                    Err(err) => break Err(err),
                };
                if let Some(tool_future) = output_result.tool_future {
                    in_flight.push_back(tool_future);
                }
                if suppress_external_effects
                    && output_result.provisional_history_persisted
                    && let Some(item_id) = completed_item_id.as_deref()
                    && let Some(provisional_response_item) = provisional_response_item.as_ref()
                {
                    if let Err(err) = sess
                        .services
                        .task_evidence
                        .mark_final_item_persisted(
                            &turn_context.sub_id,
                            item_id,
                            provisional_response_item,
                        )
                        .await
                    {
                        break Err(CodexErr::InvalidRequest(err));
                    }
                }
                if require_durable_lifecycle
                    && output_result.turn_item_completed
                    && output_result.last_agent_message_item_id.as_deref()
                        == completed_item_id.as_deref()
                    && let Some(item_id) = completed_item_id.as_deref()
                    && let Err(err) = sess
                        .services
                        .task_evidence
                        .mark_final_item_completed(&turn_context.sub_id, item_id)
                        .await
                {
                    break Err(CodexErr::InvalidRequest(err));
                }
                if completed_final_matches
                    && output_result.last_agent_message_item_id.as_deref()
                        == completed_item_id.as_deref()
                    && let Some(agent_message) = output_result.last_agent_message
                {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
                // todo: remove before stabilizing multi-agent v2
                if preempt_for_mailbox_mail && sess.input_queue.has_pending_mailbox_items().await {
                    break Ok(SamplingRequestResult {
                        needs_follow_up: true,
                        last_agent_message,
                        pending_managed_final: pending_managed_final.take(),
                        models_refresh_task: None,
                    });
                }
            }
            ResponseEvent::OutputItemAdded(mut item) => {
                if turn_context.item_ids_enabled()
                    || managed_task_turn
                    || is_final_assistant_response_item(&item)
                {
                    assign_missing_streamed_response_item_id(&mut item, /*active_item*/ None);
                }
                let is_final_assistant_item = is_final_assistant_response_item(&item);
                if is_final_assistant_item
                    && let Err(err) = drain_in_flight(
                        &mut in_flight,
                        Arc::clone(&sess),
                        Arc::clone(&turn_context),
                    )
                    .await
                {
                    break Err(err);
                }
                if let ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    namespace,
                    ..
                } = &item
                {
                    let tool_name = ToolName::new(namespace.clone(), name.as_str());
                    active_tool_argument_diff_consumer = tool_runtime
                        .create_diff_consumer(&tool_name)
                        .map(|consumer| (call_id.clone(), consumer));
                } else if matches!(&item, ResponseItem::FunctionCall { .. }) {
                    active_tool_argument_diff_consumer = None;
                }
                if let Some(turn_item) = handle_non_tool_response_item(
                    sess.as_ref(),
                    TurnItemContributorPolicy::Skip,
                    &item,
                    plan_mode,
                )
                .await
                {
                    let mut turn_item = turn_item;
                    let (provisional_final, final_committed, final_reserved) =
                        if is_final_assistant_item && managed_task_turn {
                            (true, false, false)
                        } else if is_final_assistant_item {
                            let reserved = sess
                                .services
                                .task_evidence
                                .authorize_final_item(&turn_context.sub_id, &turn_item.id())
                                .await
                                .unwrap_or(false);
                            let commit_now =
                                reserved && !defer_streamed_turn_items_for_contributors;
                            let committed = commit_now
                                && sess
                                    .services
                                    .task_evidence
                                    .commit_final_item(&turn_context.sub_id, &turn_item.id())
                                    .await
                                    .unwrap_or(false);
                            (
                                !reserved || (commit_now && !committed),
                                committed,
                                reserved && !commit_now,
                            )
                        } else {
                            (false, false, false)
                        };
                    let stream_item_to_client =
                        !defer_streamed_turn_items_for_contributors && !provisional_final;
                    let mut seeded_parsed: Option<ParsedAssistantTextDelta> = None;
                    let mut seeded_item_id: Option<String> = None;
                    if stream_item_to_client
                        && matches!(turn_item, TurnItem::AgentMessage(_))
                        && let Some(raw_text) = raw_assistant_output_text_from_item(&item)
                    {
                        let item_id = turn_item.id();
                        let mut seeded =
                            assistant_message_stream_parsers.seed_item_text(&item_id, &raw_text);
                        if let TurnItem::AgentMessage(agent_message) = &mut turn_item {
                            agent_message.content =
                                vec![codex_protocol::items::AgentMessageContent::Text {
                                    text: if plan_mode {
                                        String::new()
                                    } else {
                                        std::mem::take(&mut seeded.visible_text)
                                    },
                                }];
                        }
                        seeded_parsed = plan_mode.then_some(seeded);
                        seeded_item_id = Some(item_id);
                    }
                    if stream_item_to_client {
                        if let Some(state) = plan_mode_state.as_mut()
                            && matches!(turn_item, TurnItem::AgentMessage(_))
                        {
                            let item_id = turn_item.id();
                            state
                                .pending_agent_message_items
                                .insert(item_id, turn_item.clone());
                        } else {
                            sess.emit_turn_item_started(&turn_context, &turn_item).await;
                        }
                        if let (Some(state), Some(item_id), Some(parsed)) = (
                            plan_mode_state.as_mut(),
                            seeded_item_id.as_deref(),
                            seeded_parsed,
                        ) {
                            emit_streamed_assistant_text_delta(
                                &sess,
                                &turn_context,
                                Some(state),
                                item_id,
                                parsed,
                            )
                            .await;
                        }
                    }
                    active_item = Some(turn_item);
                    active_item_is_streaming_to_client = stream_item_to_client;
                    active_item_is_provisional_final = provisional_final;
                    active_item_final_committed = final_committed;
                    active_final_reservation =
                        final_reserved.then(|| active_item.as_ref().expect("active item").id());
                }
            }
            ResponseEvent::ServerModel(server_model) => {
                if !turn_context
                    .server_model_warning_emitted
                    .load(Ordering::Relaxed)
                    && sess
                        .maybe_warn_on_server_model_mismatch(&turn_context, server_model)
                        .await
                {
                    turn_context
                        .server_model_warning_emitted
                        .store(true, Ordering::Relaxed);
                }
            }
            ResponseEvent::ModelVerifications(verifications) => {
                if !turn_context
                    .model_verification_emitted
                    .swap(true, Ordering::Relaxed)
                {
                    sess.emit_model_verification(&turn_context, verifications)
                        .await;
                }
            }
            ResponseEvent::TurnModerationMetadata(metadata) => {
                sess.emit_turn_moderation_metadata(&turn_context, metadata)
                    .await;
            }
            ResponseEvent::SafetyBuffering(buffering) => {
                sess.send_event(
                    &turn_context,
                    EventMsg::SafetyBuffering(SafetyBufferingEvent {
                        model: turn_context.model_info.slug.clone(),
                        use_cases: buffering.use_cases,
                        reasons: buffering.reasons,
                        show_buffering_ui: buffering.show_buffering_ui,
                        faster_model: buffering.faster_model,
                    }),
                )
                .await;
            }
            ResponseEvent::ServerReasoningIncluded(included) => {
                sess.set_server_reasoning_included(included).await;
            }
            ResponseEvent::RateLimits(snapshot) => {
                // Update internal state with latest rate limits, but defer sending until
                // token usage is available to avoid duplicate TokenCount events.
                sess.record_rate_limits_info(snapshot).await;
                should_emit_token_count = true;
            }
            ResponseEvent::ModelsEtag(etag) => {
                latest_models_etag = Some(etag);
            }
            ResponseEvent::Completed {
                token_usage,
                end_turn,
                ..
            } => {
                flush_assistant_text_segments_all(
                    &sess,
                    &turn_context,
                    plan_mode_state.as_mut(),
                    &mut assistant_message_stream_parsers,
                )
                .await;
                let budget_result = sess
                    .record_token_usage_info(&turn_context, token_usage.as_ref())
                    .await;
                should_emit_token_count = true;
                should_emit_turn_diff = true;
                if let Err(err) = budget_result {
                    break Err(err);
                }
                if let Some(false) = end_turn {
                    needs_follow_up = true;
                }
                break Ok(SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message,
                    pending_managed_final: pending_managed_final.take(),
                    models_refresh_task: None,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                // In review child threads, suppress assistant text deltas; the
                // UI will show a selection popup from the final ReviewOutput.
                if let Some(active) = active_item.as_ref() {
                    if !active_item_is_streaming_to_client {
                        continue;
                    }
                    let item_id = active.id();
                    if matches!(active, TurnItem::AgentMessage(_)) {
                        let parsed = assistant_message_stream_parsers.parse_delta(&item_id, &delta);
                        emit_streamed_assistant_text_delta(
                            &sess,
                            &turn_context,
                            plan_mode_state.as_mut(),
                            &item_id,
                            parsed,
                        )
                        .await;
                    } else {
                        let event = AgentMessageContentDeltaEvent {
                            thread_id: sess.thread_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            item_id,
                            delta,
                        };
                        sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                            .await;
                    }
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ToolCallInputDelta {
                item_id: _,
                call_id,
                delta,
            } => {
                let Some((active_call_id, consumer)) = active_tool_argument_diff_consumer.as_mut()
                else {
                    continue;
                };
                let call_id = match call_id {
                    Some(call_id) if call_id.as_str() != active_call_id.as_str() => continue,
                    Some(call_id) => call_id,
                    None => active_call_id.clone(),
                };
                if let Some(event) = consumer.consume_diff(turn_context.as_ref(), call_id, &delta) {
                    sess.send_event(&turn_context, event).await;
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                if uses_sequential_cutoff_reasoning_summaries {
                    continue;
                }
                if let Some(active) = active_item.as_ref() {
                    if !active_item_is_streaming_to_client {
                        continue;
                    }
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.thread_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                if uses_sequential_cutoff_reasoning_summaries {
                    continue;
                }
                if let Some(active) = active_item.as_ref() {
                    if !active_item_is_streaming_to_client {
                        continue;
                    }
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDone {
                item_id,
                text,
                summary_index,
            } => {
                if !uses_sequential_cutoff_reasoning_summaries {
                    continue;
                }
                let Some(active) = active_item.as_ref() else {
                    continue;
                };
                if !active_item_is_streaming_to_client || active.id() != item_id {
                    continue;
                }
                if summary_index > 0 {
                    sess.send_event(
                        &turn_context,
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: item_id.clone(),
                            summary_index,
                        }),
                    )
                    .await;
                }
                let event = ReasoningContentDeltaEvent {
                    thread_id: sess.thread_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id,
                    delta: text,
                    summary_index,
                };
                sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                    .await;
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                if let Some(active) = active_item.as_ref() {
                    if !active_item_is_streaming_to_client {
                        continue;
                    }
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.thread_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };
    drop(sampling_timing_guard);

    if outcome.is_err() {
        abort_pending_managed_final_reservation(
            sess.as_ref(),
            turn_context.as_ref(),
            &mut pending_managed_final,
        )
        .await?;
    }

    if let Some(item_id) = active_final_reservation.take() {
        sess.services
            .task_evidence
            .abort_final_reservation(&turn_context.sub_id, &item_id)
            .await
            .map_err(CodexErr::InvalidRequest)?;
    }

    flush_assistant_text_segments_all(
        &sess,
        &turn_context,
        plan_mode_state.as_mut(),
        &mut assistant_message_stream_parsers,
    )
    .await;

    let tool_blocking_timing_guard = if in_flight.is_empty() {
        None
    } else {
        Some(turn_context.turn_timing_state.begin_tool_blocking())
    };
    if let Err(err) = drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await {
        abort_sampling_result_managed_final(sess.as_ref(), turn_context.as_ref(), &mut outcome)
            .await?;
        return Err(err);
    }
    drop(tool_blocking_timing_guard);

    if should_emit_token_count {
        // A tool call such as request_user_input can intentionally pause the turn. Emit token
        // counts only after pending tools resolve so clients do not see progress events while the
        // turn is waiting on the user. This also needs to happen before returning cancellation so
        // token usage already recorded from the completed response is still persisted.
        sess.send_token_count_event(&turn_context).await;
    }

    if cancellation_token.is_cancelled() {
        abort_sampling_result_managed_final(sess.as_ref(), turn_context.as_ref(), &mut outcome)
            .await?;
        return Err(CodexErr::TurnAborted);
    }

    if should_emit_turn_diff {
        let unified_diff = {
            let tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Some(unified_diff) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    if let Ok(result) = &outcome
        && !result.needs_follow_up
    {
        let validation_warning = {
            let tracker = turn_diff_tracker.lock().await;
            tracker
                .has_unvalidated_mutation()
                .then(|| {
                    tracker
                        .validation_freshness_status()
                        .final_warning_message()
                })
                .flatten()
                .map(str::to_string)
        };
        if let Some(message) = validation_warning {
            sess.send_event(&turn_context, EventMsg::Warning(WarningEvent { message }))
                .await;
        }
    }

    if let Some(etag) = latest_models_etag
        && let Ok(result) = &mut outcome
    {
        let models_manager = Arc::clone(&sess.services.models_manager);
        let http_client_factory = turn_context.config.http_client_factory();
        let refresh_cancellation = cancellation_token.child_token();
        result.models_refresh_task = Some(tokio::spawn(async move {
            let _ = models_manager
                .refresh_if_new_etag(etag, http_client_factory)
                .or_cancel(&refresh_cancellation)
                .await;
        }));
    }

    outcome
}

pub(crate) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    for item in responses.iter().rev() {
        if let Some(message) = last_assistant_message_from_item(item, /*plan_mode*/ false) {
            return Some(message);
        }
    }
    None
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
