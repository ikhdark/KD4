use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::apply_skill_injection_observability;
use crate::client::AttemptPreparedCallback;
use crate::client::ModelClientSession;
use crate::client::StartupPrewarmClaim;
use crate::client_common::Prompt;
use crate::client_common::PromptDigests;
use crate::client_common::ResponseEvent;
use crate::collect_explicit_skill_mentions;
use crate::compact::InitialContextInjection;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::compact_remote_v2::run_inline_remote_auto_compact_task as run_inline_remote_auto_compact_task_v2;
use crate::connectors;
use crate::context::ApprovalPromptContext;
use crate::context::ContextualUserFragment;
use crate::context::PermissionsInstructions;
use crate::context::PromptContextCategory;
use crate::context::RecommendedPluginsInstructions;
use crate::context::TaskModelGuidance;
use crate::context_manager::ContextManager;
use crate::context_manager::PreparedPromptInput;
use crate::feedback_tags;
use crate::hook_runtime::emit_hook_stop_reason;
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
use crate::mcp_tool_exposure::resolve_selected_skill_mcp_exposure;
use crate::mentions::build_connector_slug_counts;
use crate::mentions::build_skill_name_counts;
use crate::mentions::collect_explicit_app_ids;
use crate::mentions::collect_explicit_plugin_mentions;
use crate::mentions::collect_tool_mentions_from_messages;
use crate::pending_turn_plan::CompletedEffect;
use crate::pending_turn_plan::EffectImpact;
use crate::pending_turn_plan::FixedPointPlanningState;
use crate::plan_skill_injections;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::build_plugin_injections;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
use crate::session::PreviousTurnSettings;
use crate::session::TurnInput;
use crate::session::reasoning_governor::AuthoritativeWaitResolution;
use crate::session::reasoning_governor::ContinuationDisposition;
use crate::session::reasoning_governor::GenerationRequestDisposition;
use crate::session::reasoning_governor::SamplingConvergenceDecision;
use crate::session::reasoning_governor::SamplingGenerationDisposition;
use crate::session::reasoning_governor::SamplingReasoningGovernor;
use crate::session::reasoning_governor::SamplingReasoningPhase;
use crate::session::reasoning_governor::SamplingRequestSettledState;
use crate::session::reasoning_governor::SamplingRequestSignalCollector;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::stable_context::StableContextKind;
use crate::stable_context::StableContextTarget;
use crate::state::SamplingAdmission;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use crate::stream_events_utils::raw_assistant_output_text_from_item;
use crate::stream_events_utils::record_completed_response_item_with_finalized_facts;
use crate::tasks::TurnTaskResult;
use crate::tasks::completion_review::CompletionReviewTurnEvidence;
use crate::tasks::emit_compact_metric;
use crate::tool_history::ModelGenerationId;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolRouterParams;
use crate::tools::router::ToolSuggestCandidates;
use crate::tools::router::ToolSuggestPresentation;
use crate::tools::router::extension_tool_executors;
use crate::tools::spec_plan::search_tool_enabled;
use crate::tools::spec_plan::tool_suggest_enabled;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::ContinuationCause;
use crate::turn_timing::TurnLocalPhase;
use crate::turn_timing::TurnTimingGuard;
use crate::turn_timing::record_turn_ttft_metric;
use crate::util::error_or_panic;
use codex_analytics::AppInvocation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::InvocationType;
use codex_analytics::SkillInvocation;
use codex_analytics::TrackEventsContext;
use codex_analytics::TurnResolvedConfigFact;
use codex_analytics::build_track_events_context;
use codex_async_utils::OrCancelExt;
use codex_config::config_toml::AfterAgentPolicy;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_core_plugins::PluginLoadOutcome;
use codex_core_plugins::RecommendedPluginCandidatesInput;
use codex_core_skills::injection::InjectedHostSkillPrompts;
use codex_core_skills::injection::PlannedSkillInjections;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputEnvironment;
use codex_features::Feature;
use codex_memories_read::citations::parse_memory_citation;
use codex_protocol::ResponseItemId;
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
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::PlanDeltaEvent;
use codex_protocol::protocol::ReasoningContentDeltaEvent;
use codex_protocol::protocol::ReasoningRawContentDeltaEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SafetyBufferingEvent;
use codex_protocol::protocol::SamplingBoundaryItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SurfacedToolResult;
use codex_protocol::protocol::TurnDiffEvent;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_tools::DiscoverableTool;
use codex_tools::ToolName;
use codex_tools::filter_request_plugin_install_discoverable_tools_for_client;
use codex_tools::request_user_input_available_modes;
use codex_utils_path_uri::PathUri;
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
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
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
fn ordinary_continuation_cause(
    tool_result: bool,
    server_end_turn_false: bool,
    pending_input: bool,
) -> Option<ContinuationCause> {
    if tool_result {
        Some(ContinuationCause::ToolResult)
    } else if server_end_turn_false {
        Some(ContinuationCause::ServerEndTurnFalse)
    } else if pending_input {
        Some(ContinuationCause::PendingInput)
    } else {
        None
    }
}

pub(crate) async fn prepare_sampling_prompt_for_client(
    history: ContextManager,
    turn_context: &TurnContext,
    _client_session: &ModelClientSession,
    git_workspace: &crate::git_workspace::GitWorkspaceCache,
) -> PreparedPromptInput {
    let workspace_identity = git_workspace
        .workspace_evidence_identity(turn_context.config.cwd.as_path())
        .await;
    if turn_context.config.completed_tool_history_projection {
        history.prepare_for_sampling_prompt_with_completed_tool_projection(
            &turn_context.model_info.input_modalities,
            StableContextTarget::Sampling,
            workspace_identity.as_ref(),
            git_workspace,
        )
    } else {
        history.prepare_for_sampling_prompt_with_workspace_freshness(
            &turn_context.model_info.input_modalities,
            StableContextTarget::Sampling,
            workspace_identity.as_ref(),
            git_workspace,
        )
    }
}

pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    input: Vec<TurnInput>,
    prewarmed_client_session: Option<ModelClientSession>,
    cancellation_token: CancellationToken,
) -> CodexResult<TurnTaskResult> {
    let mut completion_review_state =
        crate::tasks::completion_review::CompletionReviewState::default();
    let mut mutating_finalizer_ran = false;
    let mut preparation_timing_guard = Some(
        turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Preparation),
    );
    let mut client_session =
        prewarmed_client_session.unwrap_or_else(|| sess.services.model_client.new_session());
    if sess.reference_context_item().await.is_none() {
        client_session
            .invalidate_provider_history_inheritance("realized context baseline is unknown");
    }
    let planning_timing_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::Planning);
    let mut fixed_point = FixedPointPlanningState::default();
    let pending_turn_plan_result = loop {
        let pending_turn_plan = match stabilize_pending_turn_plan(
            &sess,
            &turn_context,
            &input,
            &mut client_session,
            &mut fixed_point,
            &cancellation_token,
        )
        .await
        {
            Ok(plan) => plan,
            Err(err) => break Err(err),
        };
        let (world_state, display_roots) = tokio::join!(
            sess.compare_and_record_context_updates(
                pending_turn_plan.step_context.as_ref(),
                pending_turn_plan.planning_generation,
            ),
            turn_diff_display_roots(sess.as_ref(), turn_context.as_ref()),
        );
        let Some(world_state) = world_state else {
            turn_context
                .turn_timing_state
                .record_planning_invalidation();
            continue;
        };
        break Ok((pending_turn_plan, world_state, display_roots));
    };
    drop(planning_timing_guard);
    let (pending_turn_plan, mut world_state, display_roots) = match pending_turn_plan_result {
        Ok(committed) => committed,
        Err(err) => {
            if matches!(err, CodexErr::TurnAborted) {
                run_hooks_and_record_inputs(&sess, &turn_context, &input).await;
                return Err(err);
            }
            let error = err.to_codex_protocol_error();
            sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                .await;
            error!("Pending-turn planning failed before persistence or model send: {err}");
            return Ok(TurnTaskResult::default());
        }
    };
    let PendingTurnPlan {
        step_context: first_step_context,
        first_router,
        injection_items,
        explicitly_enabled_connectors,
        skill_plan,
        ..
    } = pending_turn_plan;
    let selected_skill_invocations = skill_plan.invocations;

    // Pending-turn planning is stable and compare-and-commit has atomically
    // begun persistence under the session-state owner.

    if run_pending_session_start_hooks(&sess, &turn_context).await {
        return Ok(TurnTaskResult::default());
    }
    let mut can_drain_pending_input = input.is_empty();
    if run_hooks_and_record_inputs(&sess, &turn_context, &input).await {
        return Ok(TurnTaskResult::default());
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
    let mut surfaced_result: Option<SurfacedToolResult> = None;
    let mut stop_hook_active = false;
    let mut pending_continuation_cause = None;
    let mut pending_generation_request: Option<GenerationRequestDisposition> = None;
    let mut has_started_generation = false;
    let mut logical_generation_ordinal = 0_u32;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(
        TurnDiffTracker::with_environment_display_roots(display_roots),
    ));
    let mut reasoning_governor = SamplingReasoningGovernor::new_with_timing(
        turn_context.config.reasoning_phase_efforts.as_ref(),
        Arc::clone(&turn_context.turn_timing_state),
    );

    // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
    // one instance across retries within this turn.
    // Pending input is drained into history before building the next model request.
    // However, we defer that drain until after sampling in two cases:
    // 1. At the start of a turn, so the fresh turn input in `input` gets sampled first.
    // 2. After auto-compact, when model/tool continuation needs to resume before any steer.

    let mut next_step_context = Some(first_step_context);
    let mut first_router = Some(first_router);
    'sampling_loop: loop {
        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        let pending_input = if can_drain_pending_input {
            sess.input_queue.get_pending_input(&sess.active_turn).await
        } else {
            Vec::new()
        };

        let recorded_input =
            run_hooks_and_record_inputs_detailed(&sess, &turn_context, &pending_input).await;
        if recorded_input.accepted_user_input {
            reasoning_governor.accepted_user_input();
        }
        if recorded_input.should_stop {
            break;
        }

        let window_id = sess.current_window_id().await;
        // Capture once so context, advertised tools, and tool calls share one request view.
        let step_context = match next_step_context.take() {
            Some(step_context) => step_context,
            None => sess.capture_step_context(Arc::clone(&turn_context)).await,
        };
        let request_baselines = {
            let tracker = turn_diff_tracker.lock().await;
            reasoning_governor.baselines(
                tracker.current_mutation_revision(),
                tracker.validation_freshness_status(),
                tracker.last_successful_validation_revision(),
            )
        };
        let request_signals = reasoning_governor.collector(&request_baselines);
        let generation_request = pending_generation_request.take().unwrap_or_else(|| {
            if !has_started_generation {
                reasoning_governor.initial_generation_request(&request_baselines)
            } else {
                GenerationRequestDisposition {
                    purpose: match pending_continuation_cause {
                        Some(ContinuationCause::Compaction) => {
                            Some(TurnTimingGenerationPurpose::CompactionRecovery)
                        }
                        Some(ContinuationCause::CompletionReviewRepair)
                        | Some(ContinuationCause::InvalidImageRecovery)
                        | Some(ContinuationCause::StopHook) => {
                            Some(TurnTimingGenerationPurpose::Repair)
                        }
                        Some(ContinuationCause::PendingInput) => {
                            Some(TurnTimingGenerationPurpose::InitialReasoning)
                        }
                        Some(ContinuationCause::ToolResult) => {
                            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
                        }
                        Some(ContinuationCause::ServerEndTurnFalse) | None => {
                            Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
                        }
                    },
                    sampling: SamplingGenerationDisposition::DecisionBearing,
                    relevant_state_fingerprint: request_baselines.relevant_state_fingerprint(),
                    failure_fingerprint: None,
                    terminal_completion_only: false,
                }
            }
        });
        has_started_generation = true;
        let generation_id = ModelGenerationId {
            turn_id: turn_context.sub_id.clone(),
            ordinal: logical_generation_ordinal,
        };
        logical_generation_ordinal = logical_generation_ordinal.saturating_add(1);
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
            let sampling_request_input: PreparedPromptInput = async {
                let history_snapshot_guard = turn_context
                    .turn_timing_state
                    .begin_local_phase(TurnLocalPhase::HistorySnapshot);
                let history = sess.clone_history().await;
                drop(history_snapshot_guard);
                let normalization_guard = turn_context
                    .turn_timing_state
                    .begin_local_phase(TurnLocalPhase::Normalization);
                let prepared = prepare_sampling_prompt_for_client(
                    history,
                    turn_context.as_ref(),
                    &client_session,
                    sess.services.git_workspace.as_ref(),
                )
                .await;
                drop(normalization_guard);
                prepared
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
                &selected_skill_invocations,
                &mut first_router,
                &mut preparation_timing_guard,
                reasoning_governor.phase(),
                reasoning_governor.trigger(),
                generation_request.clone(),
                generation_id.clone(),
                request_signals.clone(),
                &mut pending_continuation_cause,
                cancellation_token.child_token(),
            )
            .await
        }
        .await;
        match sampling_request_result {
            Ok((sampling_request_output, sampling_request_input)) => {
                let SamplingRequestResult {
                    needs_follow_up: model_needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                    settled_state,
                    tool_result_continuation,
                    server_end_turn_false,
                } = sampling_request_output;
                reasoning_governor.settle(&request_baselines, &request_signals, &settled_state);
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
                // A proven loop receives one final, tool-free generation. Once
                // that generation returns, only newly queued user input may
                // keep the turn alive.
                let mut needs_follow_up = generation_needs_follow_up(
                    &generation_request,
                    model_needs_follow_up,
                    has_pending_input,
                );
                let progress_kinds =
                    request_signals.progress_kinds(&request_baselines, &settled_state);
                let convergence_decision = if needs_follow_up && !has_pending_input {
                    Some(reasoning_governor.evaluate_convergence(
                        &request_baselines,
                        &request_signals,
                        &settled_state,
                    ))
                } else {
                    None
                };
                let authoritative_wait_terminal_surface = convergence_decision
                    .as_ref()
                    .and_then(authoritative_wait_terminal_surface);
                let terminal_completion_required =
                    convergence_decision.as_ref().is_some_and(|decision| {
                        decision.continuation == ContinuationDisposition::TerminalCompletionRequired
                    });
                if authoritative_wait_terminal_surface.is_some() {
                    needs_follow_up = false;
                }
                let mut next_generation_request = needs_follow_up.then(|| {
                    reasoning_governor.continuation_generation_request(
                        &request_baselines,
                        &request_signals,
                        &settled_state,
                        has_pending_input,
                        server_end_turn_false && !tool_result_continuation && !has_pending_input,
                    )
                });
                if terminal_completion_required {
                    next_generation_request = next_generation_request
                        .map(GenerationRequestDisposition::require_terminal_completion);
                }
                if next_generation_request
                    .as_ref()
                    .is_some_and(|request| request.sampling.is_residual_deterministic())
                {
                    turn_context
                        .turn_timing_state
                        .record_residual_deterministic_generation();
                }
                if next_generation_request
                    .as_ref()
                    .is_some_and(
                        super::reasoning_governor::GenerationRequestDisposition::completes_protocol_turn_deterministically,
                    )
                {
                    // The only remaining action is host-owned protocol
                    // completion. Sampling here would spend an entire model
                    // generation to rediscover an already-proved action.
                    needs_follow_up = false;
                    next_generation_request = None;
                }
                turn_context.turn_timing_state.record_generation_outcome(
                    progress_kinds.clone(),
                    next_generation_request
                        .as_ref()
                        .is_some_and(|next| next.purpose != generation_request.purpose),
                    progress_kinds.is_empty(),
                );
                pending_generation_request = next_generation_request;
                if request_signals.is_wait_only() {
                    turn_context.turn_timing_state.record_wait_only_generation();
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

                // as long as compaction works well in getting us way below the token limit, we shouldn't worry about being in an infinite loop.
                if needs_follow_up && token_limit_reached {
                    if let Err(err) = run_auto_compact(
                        &sess,
                        Arc::clone(&step_context),
                        /*fallback_step_context*/ None,
                        &mut client_session,
                        InitialContextInjection::AtStart(Arc::clone(&world_state)),
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
                        return Ok(TurnTaskResult::default());
                    }
                    can_drain_pending_input = !model_needs_follow_up;
                    reasoning_governor.host_retain();
                    pending_generation_request = None;
                    pending_continuation_cause = Some(ContinuationCause::Compaction);
                    continue;
                }

                if !needs_follow_up {
                    if let Some(authoritative_result) = authoritative_wait_terminal_surface {
                        last_agent_message = authoritative_result.canonical_message.clone();
                        surfaced_result = Some(authoritative_result);
                    } else {
                        last_agent_message = sampling_request_last_agent_message;
                    }
                    let observed_stop_outcome = run_turn_stop_hooks(
                        &sess,
                        &turn_context,
                        stop_hook_active,
                        last_agent_message.clone(),
                    )
                    .await;
                    if let Some(reason) = observed_stop_outcome.observation_error {
                        let turn_state = {
                            let active_turn = sess.active_turn.lock().await;
                            active_turn
                                .as_ref()
                                .map(|active_turn| Arc::clone(&active_turn.turn_state))
                        };
                        if let Some(turn_state) = turn_state {
                            turn_state
                                .lock()
                                .await
                                .record_completion_review_partial_reason(format!(
                                    "Stop completion observation failed: {reason}"
                                ));
                        }
                    }
                    if observed_stop_outcome.workspace_changed {
                        reasoning_governor.host_mutation();
                        trace!(
                            "Stop hook workspace mutation will be included in completion review"
                        );
                    }
                    let stop_outcome = observed_stop_outcome.stop;
                    if stop_outcome.should_block {
                        if let Some(hook_prompt_message) =
                            build_hook_prompt_message(&stop_outcome.continuation_fragments)
                        {
                            sess.record_response_item_and_emit_turn_item(
                                &turn_context,
                                hook_prompt_message,
                            )
                            .await;
                            stop_hook_active = true;
                            reasoning_governor.host_diagnose();
                            pending_generation_request = None;
                            pending_continuation_cause = Some(ContinuationCause::StopHook);
                            continue;
                        } else {
                            let reason = stop_outcome
                                .block_reason
                                .as_deref()
                                .filter(|reason| !reason.trim().is_empty())
                                .map(|reason| format!(": {reason}"))
                                .unwrap_or_default();
                            sess.send_event(
                                &turn_context,
                                EventMsg::Warning(WarningEvent {
                                    message: format!("Stop hook requested continuation without a prompt{reason}; ignoring the block."),
                                }),
                            )
                            .await;
                        }
                    }
                    if stop_outcome.should_stop {
                        emit_hook_stop_reason(
                            &sess,
                            &turn_context,
                            "Stop",
                            stop_outcome.stop_reason.as_deref(),
                        )
                        .await;
                        break;
                    }
                    let mutating_finalizer_aborted = if matches!(
                        turn_context.config.after_agent_policy,
                        AfterAgentPolicy::MutatingFinalizer
                    ) && !mutating_finalizer_ran
                    {
                        mutating_finalizer_ran = true;
                        let after_agent_outcome = run_legacy_after_agent_hook(
                            &sess,
                            &turn_context,
                            &sampling_request_input,
                            last_agent_message.clone(),
                        )
                        .await;
                        if let Some(reason) = after_agent_outcome.observation_error {
                            record_completion_review_partial_reason(
                                &sess,
                                format!("AfterAgent completion observation failed: {reason}"),
                            )
                            .await;
                        } else if after_agent_outcome.workspace_changed {
                            reasoning_governor.host_mutation();
                        }
                        if after_agent_outcome.aborted {
                            record_completion_review_partial_reason(
                                &sess,
                                "the reviewed candidate changed during terminal finalization"
                                    .to_string(),
                            )
                            .await;
                        }
                        after_agent_outcome.aborted
                    } else {
                        false
                    };
                    let completion_review_turn_evidence = {
                        let tracker = turn_diff_tracker.lock().await;
                        CompletionReviewTurnEvidence {
                            exact_diff: tracker.get_unified_diff(),
                            mutation_revision: tracker.current_mutation_revision(),
                            validation_freshness: tracker.validation_freshness_status(),
                            last_successful_validation_revision: tracker
                                .last_successful_validation_revision(),
                        }
                    };
                    let review_outcome = Box::pin(
                        crate::tasks::completion_review::coordinate_completion_review(
                            &sess,
                            &turn_context,
                            &cancellation_token,
                            &completion_review_turn_evidence,
                            last_agent_message.as_deref(),
                            &mut completion_review_state,
                        ),
                    )
                    .await?;
                    let review_report =
                        report_completion_review_outcome(&sess, &turn_context, review_outcome)
                            .await;
                    if review_report.repair_injected {
                        if mutating_finalizer_aborted {
                            return Ok(TurnTaskResult::default());
                        }
                        reasoning_governor.host_diagnose();
                        pending_generation_request = None;
                        pending_continuation_cause =
                            Some(ContinuationCause::CompletionReviewRepair);
                        continue 'sampling_loop;
                    }
                    if matches!(
                        turn_context.config.after_agent_policy,
                        AfterAgentPolicy::MutatingFinalizer
                    ) {
                        if mutating_finalizer_aborted {
                            return Ok(TurnTaskResult::default());
                        }
                        break;
                    }
                    let correction_consumed = sess
                        .services
                        .task_evidence
                        .completion_review_correction_consumed()
                        .await;
                    let after_agent_outcome = run_legacy_after_agent_hook(
                        &sess,
                        &turn_context,
                        &sampling_request_input,
                        last_agent_message.clone(),
                    )
                    .await;
                    if let Some(reason) = after_agent_outcome.observation_error {
                        record_completion_review_partial_reason(
                            &sess,
                            format!("AfterAgent completion observation failed: {reason}"),
                        )
                        .await;
                    } else if after_agent_outcome.workspace_changed {
                        reasoning_governor.host_mutation();
                        if review_report.provisional_clean
                            && !matches!(
                                sess.services
                                    .task_evidence
                                    .prepare_after_agent_completion_review_reentry(
                                        correction_consumed,
                                    )
                                    .await,
                                crate::task_evidence::AtomicReviewTransition::Persisted(())
                            )
                        {
                            record_completion_review_partial_reason(
                                &sess,
                                "the completion review could not durably re-enter after AfterAgent mutation"
                                    .to_string(),
                            )
                            .await;
                        } else if review_report.provisional_clean {
                            if after_agent_outcome.aborted {
                                return Ok(TurnTaskResult::default());
                            }

                            let observed_stop_outcome = run_turn_stop_hooks(
                                &sess,
                                &turn_context,
                                stop_hook_active,
                                last_agent_message.clone(),
                            )
                            .await;
                            if let Some(reason) = observed_stop_outcome.observation_error {
                                record_completion_review_partial_reason(
                                    &sess,
                                    format!("Stop completion observation failed: {reason}"),
                                )
                                .await;
                            }
                            if observed_stop_outcome.workspace_changed {
                                reasoning_governor.host_mutation();
                                trace!(
                                    "Stop hook workspace mutation will be included in refreshed completion review"
                                );
                            }
                            let stop_outcome = observed_stop_outcome.stop;
                            if stop_outcome.should_block {
                                if let Some(hook_prompt_message) =
                                    build_hook_prompt_message(&stop_outcome.continuation_fragments)
                                {
                                    sess.record_response_item_and_emit_turn_item(
                                        &turn_context,
                                        hook_prompt_message,
                                    )
                                    .await;
                                    stop_hook_active = true;
                                    reasoning_governor.host_diagnose();
                                    pending_generation_request = None;
                                    pending_continuation_cause = Some(ContinuationCause::StopHook);
                                    continue 'sampling_loop;
                                } else {
                                    let reason = stop_outcome
                                        .block_reason
                                        .as_deref()
                                        .filter(|reason| !reason.trim().is_empty())
                                        .map(|reason| format!(": {reason}"))
                                        .unwrap_or_default();
                                    sess.send_event(
                                        &turn_context,
                                        EventMsg::Warning(WarningEvent {
                                            message: format!("Stop hook requested continuation without a prompt{reason}; ignoring the block."),
                                        }),
                                    )
                                    .await;
                                }
                            }
                            if stop_outcome.should_stop {
                                emit_hook_stop_reason(
                                    &sess,
                                    &turn_context,
                                    "Stop",
                                    stop_outcome.stop_reason.as_deref(),
                                )
                                .await;
                                break 'sampling_loop;
                            }

                            completion_review_state =
                                crate::tasks::completion_review::CompletionReviewState::default();
                            let completion_review_turn_evidence = {
                                let tracker = turn_diff_tracker.lock().await;
                                CompletionReviewTurnEvidence {
                                    exact_diff: tracker.get_unified_diff(),
                                    mutation_revision: tracker.current_mutation_revision(),
                                    validation_freshness: tracker.validation_freshness_status(),
                                    last_successful_validation_revision: tracker
                                        .last_successful_validation_revision(),
                                }
                            };
                            let refreshed_review = Box::pin(
                                crate::tasks::completion_review::coordinate_completion_review(
                                    &sess,
                                    &turn_context,
                                    &cancellation_token,
                                    &completion_review_turn_evidence,
                                    last_agent_message.as_deref(),
                                    &mut completion_review_state,
                                ),
                            )
                            .await?;
                            let refreshed_review_report = report_completion_review_outcome(
                                &sess,
                                &turn_context,
                                refreshed_review,
                            )
                            .await;
                            if refreshed_review_report.repair_injected {
                                reasoning_governor.host_diagnose();
                                pending_generation_request = None;
                                pending_continuation_cause =
                                    Some(ContinuationCause::CompletionReviewRepair);
                                continue 'sampling_loop;
                            }
                            trace!(
                                provisional_clean = refreshed_review_report.provisional_clean,
                                "refreshed completion review outcome recorded"
                            );
                        }
                    }
                    if after_agent_outcome.aborted {
                        return Ok(TurnTaskResult::default());
                    }
                    break;
                }
                pending_continuation_cause = ordinary_continuation_cause(
                    tool_result_continuation,
                    server_end_turn_false,
                    has_pending_input,
                );
                if let Some(decision) = convergence_decision {
                    if decision.proven_loop_activated {
                        turn_context
                            .turn_timing_state
                            .record_proven_loop_activation();
                    }
                    if let Some(directive) = decision.directive {
                        turn_context
                            .turn_timing_state
                            .record_no_progress_directive();
                        let directive_item = ResponseItem::Message {
                            id: None,
                            role: "developer".to_string(),
                            content: vec![ContentItem::InputText { text: directive }],
                            phase: None,
                            internal_chat_message_metadata_passthrough: None,
                        };
                        sess.record_conversation_items(
                            &turn_context,
                            std::slice::from_ref(&directive_item),
                        )
                        .await;
                    }
                }
                debug_assert!(pending_continuation_cause.is_some());
                continue;
            }
            Err(err @ CodexErr::TurnAborted) => {
                return Err(err);
            }
            Err(codex_error @ CodexErr::InvalidImageRequest()) => {
                {
                    let mut state = sess.state.lock().await;
                    error_or_panic(
                        "Invalid image detected; sanitizing tool output to prevent poisoning",
                    );
                    if state.history.replace_last_turn_images("Invalid image") {
                        pending_generation_request = None;
                        pending_continuation_cause = Some(ContinuationCause::InvalidImageRecovery);
                        continue;
                    }
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

    Ok(TurnTaskResult {
        last_agent_message,
        surfaced_result,
    })
}

fn authoritative_wait_terminal_surface(
    decision: &SamplingConvergenceDecision,
) -> Option<SurfacedToolResult> {
    if decision.continuation != ContinuationDisposition::SurfaceExistingResult {
        return None;
    }
    match &decision.authoritative_wait {
        Some(AuthoritativeWaitResolution::Terminal(result)) => Some(SurfacedToolResult {
            adapter: result.adapter.clone(),
            value: result.value.clone(),
            canonical_message: result.surfaceable_message.clone(),
        }),
        Some(AuthoritativeWaitResolution::Blocked(_)) | None => None,
    }
}

fn generation_needs_follow_up(
    generation_request: &GenerationRequestDisposition,
    model_needs_follow_up: bool,
    has_pending_input: bool,
) -> bool {
    if generation_request.terminal_completion_only {
        has_pending_input
    } else {
        model_needs_follow_up || has_pending_input
    }
}

async fn record_completion_review_partial_reason(sess: &Session, reason: String) {
    let turn_state = {
        let active_turn = sess.active_turn.lock().await;
        active_turn
            .as_ref()
            .map(|active_turn| Arc::clone(&active_turn.turn_state))
    };
    if let Some(turn_state) = turn_state {
        turn_state
            .lock()
            .await
            .record_completion_review_partial_reason(reason);
    }
}

async fn report_completion_review_outcome(
    sess: &Session,
    turn_context: &TurnContext,
    outcome: crate::tasks::completion_review::CompletionReviewCoordinatorOutcome,
) -> CompletionReviewReport {
    if let Some(warning) = outcome.advisory {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent { message: warning }),
        )
        .await;
    }
    for reason in outcome.partial_reasons {
        record_completion_review_partial_reason(sess, reason).await;
    }
    CompletionReviewReport {
        provisional_clean: outcome.provisional_clean,
        repair_injected: outcome.repair_injected,
    }
}

struct CompletionReviewReport {
    provisional_clean: bool,
    repair_injected: bool,
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
    run_hooks_and_record_inputs_detailed(sess, turn_context, input)
        .await
        .should_stop
}

struct RecordedInputOutcome {
    should_stop: bool,
    accepted_user_input: bool,
}

fn resets_reasoning_governor(input: &TurnInput) -> bool {
    matches!(input, TurnInput::UserInput { content, .. } if !content.is_empty())
}

async fn run_hooks_and_record_inputs_detailed(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
) -> RecordedInputOutcome {
    let mut blocked_input = false;
    let mut accepted_user_input = false;
    for input_item in input {
        let hook_outcome = inspect_pending_input(sess, turn_context, input_item).await;
        if hook_outcome.should_stop {
            blocked_input = true;
            emit_hook_stop_reason(
                sess,
                turn_context,
                "UserPromptSubmit",
                hook_outcome.stop_reason.as_deref(),
            )
            .await;
            record_additional_contexts(sess, turn_context, hook_outcome.additional_contexts).await;
        } else {
            if resets_reasoning_governor(input_item) {
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
    RecordedInputOutcome {
        should_stop: blocked_input && !accepted_user_input,
        accepted_user_input,
    }
}

struct PendingTurnPlan {
    planning_generation: u64,
    step_context: Arc<StepContext>,
    first_router: Arc<ToolRouter>,
    injection_items: Vec<ResponseItem>,
    explicitly_enabled_connectors: HashSet<String>,
    projected_prompt_pressure: ProjectedPromptPressure,
    mcp_dependency_effect: Option<PlannedMcpDependencyEffect>,
    warnings: Vec<String>,
    skill_plan: PlannedSkillInjections,
    tracking: TrackEventsContext,
    mentioned_apps: Vec<(String, Option<String>)>,
    mentioned_plugins: Vec<PluginCapabilitySummary>,
}

enum PendingTurnPlanBuild {
    Stale,
    Ready(Box<PendingTurnPlan>),
}

fn contains_task_term(task: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }

    task.match_indices(term).any(|(start, matched)| {
        let end = start + matched.len();
        let starts_at_boundary = task[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric());
        let ends_at_boundary = task[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric());
        starts_at_boundary && ends_at_boundary
    })
}

fn task_relevant_recommended_plugins(
    user_input: &[ContentItem],
    candidates: Vec<DiscoverableTool>,
) -> Vec<DiscoverableTool> {
    let task = user_input
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } => Some(text.as_str()),
            ContentItem::InputImage { .. } | ContentItem::OutputText { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    if task.is_empty() {
        return Vec::new();
    }

    let names_a_plugin_category = ["plugin", "plugins", "integration", "integrations"]
        .iter()
        .any(|term| contains_task_term(&task, term));
    let requests_recommendations = [
        "add",
        "available",
        "connect",
        "find",
        "install",
        "list",
        "recommend",
        "show",
        "suggest",
        "use",
        "what",
        "which",
    ]
    .iter()
    .any(|term| contains_task_term(&task, term));
    if names_a_plugin_category && requests_recommendations {
        return candidates;
    }

    candidates
        .into_iter()
        .filter(|candidate| {
            contains_task_term(&task, &candidate.name().to_lowercase())
                || contains_task_term(&task, &candidate.id().to_lowercase())
        })
        .collect()
}

async fn build_recommended_plugin_items(
    sess: &Session,
    turn_context: &TurnContext,
    loaded_plugins: &PluginLoadOutcome,
    user_input: &[ContentItem],
) -> Vec<ResponseItem> {
    if !tool_suggest_enabled(turn_context) {
        return Vec::new();
    }

    let auth = sess.services.auth_manager.auth().await;
    let plugins_config = turn_context.config.plugins_config_input();
    let Some(candidates) = sess
        .services
        .plugins_manager
        .recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
            plugins_config: &plugins_config,
            loaded_plugins,
            auth: auth.as_ref(),
            disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
            app_server_client_name: turn_context.app_server_client_name.as_deref(),
        })
        .await
    else {
        return Vec::new();
    };
    let candidates = task_relevant_recommended_plugins(user_input, candidates);
    RecommendedPluginsInstructions::from_plugins(candidates)
        .map(ContextualUserFragment::into)
        .into_iter()
        .collect()
}

#[instrument(level = "trace", skip_all)]
async fn build_pure_pending_turn_plan(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    input: &[TurnInput],
    planning_generation: u64,
    cancellation_token: &CancellationToken,
) -> CodexResult<PendingTurnPlanBuild> {
    let turn_context = step_context.turn.as_ref();
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
        let (first_router, context_update_items) = tokio::join!(
            built_tools_for_pending_turn(
                sess.as_ref(),
                step_context.as_ref(),
                &[],
                planning_generation,
                cancellation_token
            ),
            sess.estimate_context_update_items(step_context.as_ref()),
        );
        let first_router = first_router?;
        if sess.services.planning_generation() != planning_generation {
            return Ok(PendingTurnPlanBuild::Stale);
        }
        let initial_context = sess.reference_context_item().await.is_none();
        let pending_token_estimate = estimate_pending_tokens(
            input,
            &[],
            &context_update_items,
            first_router.as_ref(),
            initial_context,
        );
        let projected_prompt_pressure =
            projected_prompt_pressure(sess, turn_context, pending_token_estimate).await;
        let warnings = first_router.planning_warnings().to_vec();
        return Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
            planning_generation,
            step_context,
            first_router,
            injection_items: Vec::new(),
            explicitly_enabled_connectors: HashSet::new(),
            projected_prompt_pressure,
            mcp_dependency_effect: None,
            warnings,
            skill_plan: PlannedSkillInjections::default(),
            tracking,
            mentioned_apps: Vec::new(),
            mentioned_plugins: Vec::new(),
        })));
    }

    // Read-only DAG roots P and E are independent. Extension contributors poll
    // concurrently internally and collect by registration index.
    let plugins_config_input = turn_context.config.plugins_config_input();
    let (loaded_plugins, extension_injection_items) = tokio::join!(
        sess.services
            .plugins_manager
            .plugins_for_config(&plugins_config_input),
        build_extension_turn_input_items(
            sess,
            step_context.as_ref(),
            &user_input,
            cancellation_token
        )
    );
    let extension_injection_items = extension_injection_items?;
    // DAG edge P -> plugin mentions. Connector inventory C waits for P because
    // plugin mentions can make inventory necessary even when apps are disabled.
    let mentioned_plugins =
        collect_explicit_plugin_mentions(&user_input, loaded_plugins.capability_summaries());
    let recommended_plugin_input = user_input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } => Some(ContentItem::InputText { text: text.clone() }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let recommended_plugin_items = build_recommended_plugin_items(
        sess,
        turn_context,
        &loaded_plugins,
        &recommended_plugin_input,
    )
    .await;
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
    let rendered_skill_items = skill_plan
        .injections
        .items
        .iter()
        .map(|skill| {
            let fragment = crate::context::SkillInstructions::from(skill);
            (fragment.role(), fragment.render())
        })
        .collect::<Vec<_>>();
    let skill_connector_items = rendered_skill_items
        .iter()
        .map(|(role, text)| {
            ContextualUserFragment::into(RenderedContextFragment::new(role, text.clone()))
        })
        .collect::<Vec<_>>();
    let skill_connector_ids = collect_explicit_app_ids_from_skill_items(
        &skill_connector_items,
        &available_connectors,
        &skill_name_counts_lower,
    );
    let skill_items = build_bounded_skill_context_items(rendered_skill_items);
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
        Some(injected_host_skill_prompts) => build_bounded_skill_context_items(
            skill_plan
                .injections
                .items
                .iter()
                .filter(|skill| !injected_host_skill_prompts.contains_path(&skill.path))
                .map(|skill| {
                    let fragment = crate::context::SkillInstructions::from(skill);
                    (fragment.role(), fragment.render())
                }),
        ),
        None => skill_items,
    };
    injection_items.insert(0, ContextualUserFragment::into(TaskModelGuidance));
    injection_items.extend(recommended_plugin_items);
    injection_items.extend(plugin_items);
    injection_items.extend(extension_injection_items);

    // Final read-only DAG leaves build the router and context estimate concurrently,
    // then validate the generation before accepting either.
    let (first_router, context_update_items) = tokio::join!(
        built_tools_for_pending_turn(
            sess.as_ref(),
            step_context.as_ref(),
            &skill_plan.invocations,
            planning_generation,
            cancellation_token,
        ),
        sess.estimate_context_update_items(step_context.as_ref()),
    );
    let first_router = first_router?;
    if sess.services.planning_generation() != planning_generation {
        return Ok(PendingTurnPlanBuild::Stale);
    }
    let mut warnings = planned_mcp.warnings;
    warnings.extend(first_router.planning_warnings().iter().cloned());
    warnings.extend(skill_plan.injections.warnings.iter().cloned());
    let initial_context = sess.reference_context_item().await.is_none();
    let pending_token_estimate = estimate_pending_tokens(
        input,
        &injection_items,
        &context_update_items,
        first_router.as_ref(),
        initial_context,
    );
    let projected_prompt_pressure =
        projected_prompt_pressure(sess, turn_context, pending_token_estimate).await;
    Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
        planning_generation,
        step_context,
        first_router,
        projected_prompt_pressure,
        injection_items,
        explicitly_enabled_connectors,
        mcp_dependency_effect: planned_mcp.effect,
        warnings,
        skill_plan,
        tracking,
        mentioned_apps,
        mentioned_plugins,
    })))
}

async fn stabilize_pending_turn_plan(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
    client_session: &mut ModelClientSession,
    fixed_point: &mut FixedPointPlanningState,
    cancellation_token: &CancellationToken,
) -> CodexResult<PendingTurnPlan> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    let mut check_previous_model_compaction = true;
    let mut incoming_precompaction_completed = false;
    loop {
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }

        // Model-transition compaction is independent of pending input. Context-pressure
        // compaction waits for the absolute next-prompt projection built below.
        let compaction_timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Compaction);
        let history_compaction = run_history_pre_sampling_compact(
            sess,
            turn_context,
            client_session,
            check_previous_model_compaction,
        )
        .await?;
        drop(compaction_timing_guard);
        check_previous_model_compaction = false;
        if history_compaction.reason.is_some() {
            client_session.invalidate_incremental_history("compaction");
            turn_context
                .turn_timing_state
                .record_planning_invalidation();
            continue;
        }

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

        let planning_generation = sess.services.planning_generation();
        let step_context = sess.capture_step_context(Arc::clone(turn_context)).await;
        let plan = match build_pure_pending_turn_plan(
            sess,
            step_context,
            input,
            planning_generation,
            cancellation_token,
        )
        .await?
        {
            PendingTurnPlanBuild::Stale => continue,
            PendingTurnPlanBuild::Ready(plan) => *plan,
        };
        fixed_point
            .begin_iteration()
            .map_err(|message| planning_failure_with_timing(turn_context, message))?;
        turn_context
            .turn_timing_state
            .record_planning_fixed_point_iteration();

        // Compaction is maintenance of the pre-existing history, not persistence
        // of this pending turn. Any compaction invalidates this pure plan and loops
        // through a fresh snapshot before effects are applied.
        let compaction_timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Compaction);
        let compaction_reason = run_pending_input_pre_sampling_compact(
            sess,
            turn_context,
            client_session,
            plan.projected_prompt_pressure,
            !incoming_precompaction_completed,
        )
        .await?;
        drop(compaction_timing_guard);
        if compaction_reason.is_some() {
            incoming_precompaction_completed = true;
        }
        if compaction_reason.is_some() {
            client_session.invalidate_incremental_history("compaction");
            turn_context
                .turn_timing_state
                .record_planning_invalidation();
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
                    planning_failure_with_timing(
                        turn_context,
                        format!("effect `{}` failed: {err}", effect.id),
                    )
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
                .map_err(|message| planning_failure_with_timing(turn_context, message))?;
            turn_context
                .turn_timing_state
                .record_planning_semantic_effect();
            if impact.invalidates_snapshot() {
                client_session.invalidate_incremental_history("model-visible planning effect");
                turn_context
                    .turn_timing_state
                    .record_planning_invalidation();
                fixed_point
                    .require_generation_advance(
                        plan.planning_generation,
                        sess.services.planning_generation(),
                        impact,
                    )
                    .map_err(|message| planning_failure_with_timing(turn_context, message))?;
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
                .map_err(|message| planning_failure_with_timing(turn_context, message))?;
            turn_context
                .turn_timing_state
                .record_planning_semantic_effect();
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
                    .map_err(|message| planning_failure_with_timing(turn_context, message))?;
                turn_context
                    .turn_timing_state
                    .record_planning_semantic_effect();
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
                    .map_err(|message| planning_failure_with_timing(turn_context, message))?;
                turn_context
                    .turn_timing_state
                    .record_planning_semantic_effect();
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
                    .map_err(|message| planning_failure_with_timing(turn_context, message))?;
                turn_context
                    .turn_timing_state
                    .record_planning_semantic_effect();
            }
        }

        if sess.services.planning_generation() != plan.planning_generation {
            turn_context
                .turn_timing_state
                .record_planning_invalidation();
            continue;
        }
        return Ok(plan);
    }
}

fn planning_failure(message: impl Into<String>) -> CodexErr {
    CodexErr::Stream(
        format!("pending-turn planning failure: {}", message.into()),
        None,
    )
}

fn planning_failure_with_timing(
    turn_context: &TurnContext,
    message: impl Into<String>,
) -> CodexErr {
    turn_context.turn_timing_state.record_planning_failure();
    planning_failure(message)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTokenEstimate {
    total_tokens: i64,
    body_growth_tokens: i64,
}

fn estimate_pending_tokens(
    input: &[TurnInput],
    injection_items: &[ResponseItem],
    context_update_items: &[ResponseItem],
    router: &ToolRouter,
    initial_context: bool,
) -> PendingTokenEstimate {
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
    let injection_bytes = serde_json::to_vec(injection_items)
        .map(|value| value.len())
        .unwrap_or_default();
    let context_update_bytes = serde_json::to_vec(context_update_items)
        .map(|value| value.len())
        .unwrap_or_default();
    let tool_bytes = serde_json::to_vec(&router.model_visible_specs())
        .map(|value| value.len())
        .unwrap_or_default();
    let total_bytes = input_bytes
        .saturating_add(injection_bytes)
        .saturating_add(context_update_bytes)
        .saturating_add(tool_bytes);
    let context_growth_bytes = if initial_context {
        0
    } else {
        context_update_bytes
    };
    let body_growth_bytes = input_bytes
        .saturating_add(injection_bytes)
        .saturating_add(context_growth_bytes);
    PendingTokenEstimate {
        total_tokens: i64::try_from(total_bytes.div_ceil(4)).unwrap_or(i64::MAX),
        body_growth_tokens: i64::try_from(body_growth_bytes.div_ceil(4)).unwrap_or(i64::MAX),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectedPromptPressure {
    total_tokens: i64,
    auto_compact_scope_tokens: i64,
}

async fn projected_prompt_pressure(
    sess: &Session,
    turn_context: &TurnContext,
    pending_token_estimate: PendingTokenEstimate,
) -> ProjectedPromptPressure {
    let active_context_tokens = sess.get_total_token_usage().await;
    let committed_history_tokens = sess
        .get_estimated_token_count(turn_context)
        .await
        .unwrap_or(active_context_tokens);
    let total_tokens = projected_prompt_tokens_from_estimates(
        active_context_tokens,
        committed_history_tokens,
        pending_token_estimate.total_tokens,
    );
    let auto_compact_scope_tokens = match turn_context.config.model_auto_compact_token_limit_scope {
        AutoCompactTokenLimitScope::Total => total_tokens,
        AutoCompactTokenLimitScope::BodyAfterPrefix => {
            let baseline = sess
                .auto_compact_window_snapshot()
                .await
                .prefill_input_tokens
                .unwrap_or(active_context_tokens);
            active_context_tokens
                .saturating_sub(baseline)
                .saturating_add(pending_token_estimate.body_growth_tokens)
        }
    };
    ProjectedPromptPressure {
        total_tokens,
        auto_compact_scope_tokens,
    }
}

fn projected_prompt_tokens_from_estimates(
    active_context_tokens: i64,
    committed_history_tokens: i64,
    pending_token_estimate: i64,
) -> i64 {
    committed_history_tokens
        .saturating_add(pending_token_estimate)
        .max(active_context_tokens)
}

fn build_bounded_skill_context_items(
    rendered_skill_items: impl IntoIterator<Item = (&'static str, String)>,
) -> Vec<ResponseItem> {
    let mut budget = ModelContextBudget::default();
    rendered_skill_items
        .into_iter()
        .filter_map(|(role, text)| {
            budget
                .take(&text)
                .map(|text| ContextualUserFragment::into(RenderedContextFragment::new(role, text)))
        })
        .collect()
}

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(user_input_count = user_input.len())
)]
async fn build_extension_turn_input_items(
    sess: &Arc<Session>,
    step_context: &StepContext,
    user_input: &[UserInput],
    cancellation_token: &CancellationToken,
) -> CodexResult<Vec<ResponseItem>> {
    let turn_context = step_context.turn.as_ref();
    let contributors = sess.services.extensions.turn_input_contributors().to_vec();
    if contributors.is_empty() {
        return Ok(Vec::new());
    }

    let environments = turn_context
        .environments
        .turn_environments
        .iter()
        .enumerate()
        .map(|(index, environment)| TurnInputEnvironment {
            environment_id: environment.environment_id.clone(),
            cwd: environment.cwd().clone(),
            is_primary: index == 0,
        })
        .collect::<Vec<_>>();

    let input = TurnInputContext {
        turn_id: turn_context.sub_id.to_string(),
        user_input: user_input.to_vec(),
        environments,
        ready_selected_capability_roots: step_context
            .selected_capability_roots
            .iter()
            .map(|root| root.selected_root().clone())
            .collect(),
    };

    // Contributors are independent read-only DAG leaves. FuturesOrdered polls
    // them concurrently while preserving registration order in the result.
    let mut pending = FuturesOrdered::new();
    let deadline = tokio::time::Instant::now() + EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
    let session_extension_data = &sess.services.session_extension_data;
    let thread_extension_data = &sess.services.thread_extension_data;
    let turn_extension_data = turn_context.extension_data.as_ref();
    for (contributor_index, contributor) in contributors.into_iter().enumerate() {
        let input = input.clone();
        pending.push_back(async move {
            let contribution = contributor
                .contribute(
                    input,
                    session_extension_data,
                    thread_extension_data,
                    turn_extension_data,
                )
                .or_cancel(cancellation_token);
            match tokio::time::timeout_at(deadline, contribution).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        contributor_index,
                        scope = "turn_input",
                        timeout = ?EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT,
                        "extension turn-input contributor timed out; omitting its fragments"
                    );
                    Ok(Vec::new())
                }
            }
        });
    }
    let mut items = Vec::new();
    let mut budget = ModelContextBudget::default();
    while let Some(contributed_fragments) = pending.next().await {
        let contributed_fragments = contributed_fragments?;
        items.extend(contributed_fragments.into_iter().filter_map(|fragment| {
            let role = fragment.role();
            budget
                .take(&fragment.render())
                .map(|text| ContextualUserFragment::into(RenderedContextFragment::new(role, text)))
        }));
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
    ProjectedContextLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HistoryPreSamplingCompaction {
    reason: Option<PreSamplingCompactionReason>,
}

#[instrument(level = "trace", skip_all)]
async fn run_history_pre_sampling_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    check_previous_model: bool,
) -> CodexResult<HistoryPreSamplingCompaction> {
    if check_previous_model
        && maybe_run_previous_model_inline_compact(sess, turn_context, client_session).await?
    {
        return Ok(HistoryPreSamplingCompaction {
            reason: Some(PreSamplingCompactionReason::PreviousModel),
        });
    }
    Ok(HistoryPreSamplingCompaction { reason: None })
}

#[instrument(level = "trace", skip_all)]
async fn run_pending_input_pre_sampling_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    projected_prompt_pressure: ProjectedPromptPressure,
    allow_pending_input_compaction: bool,
) -> CodexResult<Option<PreSamplingCompactionReason>> {
    // Compare one absolute projection of the next prompt against the limits. Adding the pure
    // plan's full tool schema estimate to the previous server usage double-counts the stable tool
    // prefix and causes premature compaction churn.
    let token_status = super::context_window::projected_context_window_token_status(
        sess.as_ref(),
        turn_context.as_ref(),
        projected_prompt_pressure.total_tokens,
        projected_prompt_pressure.auto_compact_scope_tokens,
    )
    .await;
    if !allow_pending_input_compaction || !token_status.token_limit_reached {
        return Ok(None);
    }

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
    Ok(Some(PreSamplingCompactionReason::ProjectedContextLimit))
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
    let initial_context_injection = match initial_context_injection {
        InitialContextInjection::DoNotInject => {
            let world_state =
                Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await);
            InitialContextInjection::AtStart(world_state)
        }
        initial_context_injection => initial_context_injection,
    };
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
    input: impl Into<Arc<[ResponseItem]>>,
    router: &ToolRouter,
    turn_context: &TurnContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    let input = input.into();
    Prompt {
        input: Arc::clone(&input),
        stable_context_fallback_input: Arc::clone(&input),
        tool_history_fallback_input: Arc::clone(&input),
        stable_context_tool_history_fallback_input: input,
        tool_history_substitutions: Arc::from([]),
        stable_context_fallback_tool_history_substitutions: Arc::from([]),
        stable_context_manifest: Default::default(),
        prompt_provenance: Default::default(),
        digests: PromptDigests::default(),
        tools: router.model_visible_specs(),
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        base_instructions,
        output_schema: turn_context.final_output_json_schema.clone(),
        output_schema_strict: !crate::guardian::is_guardian_reviewer_source(
            &turn_context.session_source,
        ),
    }
}

pub(crate) fn build_projected_prompt(
    sess: &Session,
    prepared: &PreparedPromptInput,
    router: &ToolRouter,
    step_context: &StepContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    let input = prepared.shared_items();
    let fallback_input = prepared.shared_fallback_items();
    let tool_history_fallback_input = prepared.shared_unreplaced_items();
    let stable_context_tool_history_fallback_input = prepared.shared_unreplaced_fallback_items();
    let tools = router.model_visible_specs();
    let tool_bytes = serde_json::to_vec(&tools).unwrap_or_default();
    let input_bytes = serde_json::to_vec(&input).unwrap_or_default();
    let mut manifest = prepared.stable_context_manifest().with_repository_identity(
        step_context.loaded_agents_md.as_deref().map(|loaded| {
            loaded.stable_context_metadata(&PathUri::from_abs_path(&step_context.turn.config.cwd))
        }),
    );
    let stable_input_bytes = manifest.projected_bytes();
    let stable_input_tokens = manifest.projected_tokens();
    manifest = manifest
        .with_base_model(&step_context.turn.model_info.slug, &base_instructions.text)
        .add_component_bytes(StableContextKind::ToolSchemas, "tool_schemas", &tool_bytes);
    let dynamic_bytes = u64::try_from(input_bytes.len())
        .unwrap_or(u64::MAX)
        .saturating_sub(stable_input_bytes);
    let dynamic_tokens = i64::try_from(codex_utils_output_truncation::approx_token_count(
        std::str::from_utf8(&input_bytes).unwrap_or_default(),
    ))
    .unwrap_or(i64::MAX)
    .saturating_sub(stable_input_tokens)
    .max(0);
    manifest = manifest.add_measured_component(
        StableContextKind::DynamicHistory,
        "dynamic_history",
        &input_bytes,
        dynamic_bytes,
        dynamic_tokens,
    );
    if tool_bytes
        .windows(b"request_user_input".len())
        .any(|window| window == b"request_user_input")
    {
        manifest = manifest.add_measured_component(
            StableContextKind::RequestUserInput,
            "request_user_input_available",
            b"request_user_input",
            0,
            0,
        );
    }
    let digests = PromptDigests {
        instructions: manifest.active_content_hash(StableContextKind::BaseModel),
        tools: manifest.active_content_hash(StableContextKind::ToolSchemas),
        history: prepared.fingerprint(),
    };
    if tool_bytes
        .windows(b"wait".len())
        .any(|window| window == b"wait")
    {
        manifest = manifest.add_measured_component(
            StableContextKind::Wait,
            "wait_available",
            b"wait",
            0,
            0,
        );
    }
    let mut prompt_provenance = if step_context.turn.config.include_permissions_instructions {
        let exec_policy = sess.services.exec_policy.current();
        let permissions = PermissionsInstructions::from_permission_profile(
            &step_context.turn.permission_profile,
            step_context.turn.approval_policy.value(),
            ApprovalPromptContext::new(
                step_context.turn.config.approvals_reviewer,
                step_context
                    .turn
                    .model_info
                    .model_messages
                    .as_ref()
                    .and_then(|messages| messages.approvals.as_ref()),
            ),
            exec_policy.as_ref(),
            #[allow(deprecated)]
            &step_context.turn.cwd,
            step_context
                .turn
                .config
                .features
                .enabled(Feature::ExecPermissionApprovals),
            step_context
                .turn
                .config
                .features
                .enabled(Feature::RequestPermissionsTool),
        )
        .render();
        prepared.prompt_provenance().with_exact_fragment(
            &input,
            &permissions,
            PromptContextCategory::EnvironmentPermissions,
        )
    } else {
        prepared.prompt_provenance().clone()
    };
    if let Some(role_policy) = crate::session::multi_agents::usage_hint_text(
        &step_context.turn,
        &step_context.turn.session_source,
    ) {
        prompt_provenance = prompt_provenance.with_exact_fragment(
            &input,
            role_policy,
            PromptContextCategory::AgentRole,
        );
    }
    if matches!(step_context.turn.session_source, SessionSource::VSCode)
        && let Some(desktop_context) = step_context.turn.developer_instructions.as_deref()
    {
        prompt_provenance = prompt_provenance.with_exact_fragment(
            &input,
            desktop_context,
            PromptContextCategory::AppDesktop,
        );
    }
    Prompt {
        input,
        stable_context_fallback_input: fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
        tool_history_substitutions: prepared.tool_history_substitutions(),
        stable_context_fallback_tool_history_substitutions: prepared
            .fallback_tool_history_substitutions(),
        stable_context_manifest: manifest,
        prompt_provenance,
        digests,
        tools,
        parallel_tool_calls: step_context.turn.model_info.supports_parallel_tool_calls,
        base_instructions,
        output_schema: step_context.turn.final_output_json_schema.clone(),
        output_schema_strict: !crate::guardian::is_guardian_reviewer_source(
            &step_context.turn.session_source,
        ),
    }
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
    prepared_input: PreparedPromptInput,
    selected_skill_invocations: &[SkillInvocation],
    prebuilt_router: &mut Option<Arc<ToolRouter>>,
    preparation_timing_guard: &mut Option<TurnTimingGuard>,
    reasoning_phase: Option<SamplingReasoningPhase>,
    reasoning_trigger: codex_protocol::protocol::ReasoningPolicyTrigger,
    generation_request: GenerationRequestDisposition,
    generation_id: ModelGenerationId,
    request_signals: SamplingRequestSignalCollector,
    pending_continuation_cause: &mut Option<ContinuationCause>,
    cancellation_token: CancellationToken,
) -> CodexResult<(SamplingRequestResult, Vec<ResponseItem>)> {
    let turn_context = Arc::clone(&step_context.turn);
    let cached_router = prebuilt_router.take();
    let router = match cached_router {
        Some(router)
            if finalized_router_matches_exposure(
                router.as_ref(),
                &current_tool_exposure_identity(
                    sess.as_ref(),
                    step_context.as_ref(),
                    selected_skill_invocations,
                    &cancellation_token,
                )
                .await?,
            ) =>
        {
            router
        }
        Some(_) | None => {
            built_tools(
                sess.as_ref(),
                step_context.as_ref(),
                selected_skill_invocations,
                &cancellation_token,
            )
            .await?
        }
    };
    step_context
        .set_tool_router(Arc::clone(&router))
        .map_err(|_| {
            CodexErr::Stream(
                "sampling step tool router was already finalized".to_string(),
                None,
            )
        })?;
    // Retain the finalized router for the next sampling boundary in this turn.
    // Its exposure identity is revalidated above, so genuine surface changes
    // still rebuild while ordinary tool continuations reuse the same registry.
    *prebuilt_router = Some(Arc::clone(&router));
    let base_instructions = sess.get_base_instructions().await;
    sess.persist_rollout_items(&[codex_protocol::protocol::RolloutItem::ToolManifest(
        router.tool_manifest(turn_context.as_ref()),
    )])
    .await;

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&sess),
        Arc::clone(&step_context),
        Arc::clone(&turn_diff_tracker),
    )
    .with_sampling_request_signals(request_signals.clone());
    let _code_mode_worker = sess.services.code_mode_service.start_turn_worker(
        &sess,
        Arc::clone(&step_context),
        Arc::clone(&turn_diff_tracker),
        request_signals,
    );
    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retries = 0;
    let initial_input = prepared_input.shared_items();
    let prompt_construction_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::PromptConstruction);
    let mut prompt = build_projected_prompt(
        sess.as_ref(),
        &prepared_input,
        router.as_ref(),
        step_context.as_ref(),
        base_instructions.clone(),
    );
    if generation_request.terminal_completion_only {
        prompt.tools.clear();
        prompt.parallel_tool_calls = false;
    }
    drop(prompt_construction_guard);
    turn_context
        .turn_timing_state
        .begin_model_generation_with_failure_metadata(
            pending_continuation_cause,
            &turn_context.session_source,
            generation_request.purpose,
            generation_request.timing_disposition(),
            Some(generation_request.relevant_state_fingerprint.clone()),
            generation_request.failure_fingerprint.clone(),
        );
    loop {
        let err = match try_run_sampling_request(
            tool_runtime.clone(),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_store),
            client_session,
            responses_metadata,
            Arc::clone(&turn_diff_tracker),
            &prompt,
            &generation_id,
            preparation_timing_guard,
            reasoning_phase,
            reasoning_trigger,
            generation_request.sampling.clone(),
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => {
                return Ok((output, initial_input.to_vec()));
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
        if !crate::latency_switches::shared_prompt_input_enabled() {
            let history = sess.clone_history().await;
            let retry_input = prepare_sampling_prompt_for_client(
                history,
                turn_context.as_ref(),
                client_session,
                sess.services.git_workspace.as_ref(),
            )
            .await;
            prompt = build_projected_prompt(
                sess.as_ref(),
                &retry_input,
                router.as_ref(),
                step_context.as_ref(),
                base_instructions.clone(),
            );
        }
    }
}

fn finalized_router_matches_exposure(
    router: &ToolRouter,
    current_identity: &ToolExposureIdentity,
) -> bool {
    router.exposure_identity() == current_identity
}

async fn built_tools_for_pending_turn(
    sess: &Session,
    step_context: &StepContext,
    selected_skill_invocations: &[SkillInvocation],
    planning_generation: u64,
    cancellation_token: &CancellationToken,
) -> CodexResult<Arc<ToolRouter>> {
    let prepared = sess.startup_prepared_router.take_for_first_turn().await;
    let turn_context = step_context.turn.as_ref();
    if selected_skill_invocations.is_empty()
        && let Some(prepared) = prepared
        && prepared.planning_generation == planning_generation
        && prepared.config.as_ref() == turn_context.config.as_ref()
        && prepared.dynamic_tools.as_slice() == turn_context.dynamic_tools.as_slice()
    {
        let _router_build_timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::RouterBuild);
        let current_identity = current_tool_exposure_identity(
            sess,
            step_context,
            selected_skill_invocations,
            cancellation_token,
        )
        .await?;
        if sess.services.planning_generation() == planning_generation
            && finalized_router_matches_exposure(prepared.router.as_ref(), &current_identity)
        {
            turn_context.refresh_deferred_tool_capabilities(
                prepared.router.deferred_tool_capability_revisions(),
            );
            trace!(
                planning_generation,
                "reused startup-prepared pending-turn router"
            );
            return Ok(prepared.router);
        }
    }

    built_tools(
        sess,
        step_context,
        selected_skill_invocations,
        cancellation_token,
    )
    .await
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
    selected_skill_invocations: &[SkillInvocation],
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
    let extension_tool_executors = extension_tool_executors(sess);
    let exposure_identity = derive_tool_exposure_identity(
        sess,
        step_context,
        selected_skill_invocations,
        &loaded_plugins,
        all_mcp_tools,
        &extension_tool_executors,
    )
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
    let selected_skill_mcp_exposure = resolve_selected_skill_mcp_exposure(
        selected_skill_invocations,
        &loaded_plugins,
        all_mcp_tools,
    );
    for diagnostic in &selected_skill_mcp_exposure.diagnostics {
        warn!("{diagnostic}");
    }
    let mcp_tool_exposure = build_mcp_tool_exposure(
        all_mcp_tools,
        connectors.as_deref(),
        &turn_context.config,
        search_tool_enabled(turn_context),
        &selected_skill_mcp_exposure.selection,
    );
    let mcp_tools = has_mcp_servers.then_some(mcp_tool_exposure.direct_tools);
    let deferred_mcp_tools = mcp_tool_exposure.deferred_tools;
    let router = Arc::new(ToolRouter::from_context(
        step_context,
        ToolRouterParams {
            mcp_tools,
            deferred_mcp_tools,
            tool_suggest_candidates,
            extension_tool_executors,
            dynamic_tools: turn_context.dynamic_tools.as_slice(),
            exposure_identity,
        },
        &sess.services.tool_search_handler_cache,
    ));
    step_context
        .turn
        .refresh_deferred_tool_capabilities(router.deferred_tool_capability_revisions());
    Ok(router)
}

async fn current_tool_exposure_identity(
    sess: &Session,
    step_context: &StepContext,
    selected_skill_invocations: &[SkillInvocation],
    cancellation_token: &CancellationToken,
) -> CodexResult<ToolExposureIdentity> {
    let all_mcp_tools = step_context
        .mcp_tools()
        .or_cancel(cancellation_token)
        .await?;
    let loaded_plugins = sess
        .services
        .plugins_manager
        .plugins_for_config(&step_context.turn.config.plugins_config_input())
        .await;
    let extension_tool_executors = extension_tool_executors(sess);
    Ok(derive_tool_exposure_identity(
        sess,
        step_context,
        selected_skill_invocations,
        &loaded_plugins,
        all_mcp_tools,
        &extension_tool_executors,
    )
    .await)
}

async fn derive_tool_exposure_identity(
    sess: &Session,
    step_context: &StepContext,
    selected_skill_invocations: &[SkillInvocation],
    loaded_plugins: &codex_core_plugins::PluginLoadOutcome,
    all_mcp_tools: &[codex_mcp::ToolInfo],
    extension_tool_executors: &[Arc<
        dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>,
    >],
) -> ToolExposureIdentity {
    let turn_context = step_context.turn.as_ref();
    let selected_skill_direct_mcp_entrypoints = resolve_selected_skill_mcp_exposure(
        selected_skill_invocations,
        loaded_plugins,
        all_mcp_tools,
    )
    .direct_entrypoints;
    let agent_surface_stage = agent_surface_stage(sess, turn_context);
    let wait_available = sess.services.code_mode_service.has_waitable_cells();
    let goal_surface_state = goal_surface_state(extension_tool_executors);
    let mcp_resources_available = step_context
        .mcp
        .manager()
        .has_ready_server_with_resources()
        .await;
    let tool_search_available = search_tool_enabled(turn_context);
    let available_rui_modes =
        request_user_input_available_modes(turn_context.config.features.get());
    let request_user_input_eligible = turn_context.config.experimental_request_user_input_enabled
        && !turn_context.session_source.is_non_root_agent()
        && available_rui_modes.contains(&turn_context.collaboration_mode.mode);

    ToolExposureIdentity {
        selected_skill_direct_mcp_entrypoints,
        agent_surface_stage,
        wait_available,
        goal_surface_state,
        mcp_resources_available,
        tool_search_available,
        request_user_input_eligible,
    }
}

fn agent_surface_stage(sess: &Session, turn_context: &TurnContext) -> AgentSurfaceStage {
    let spawn_eligible = match turn_context.multi_agent_version {
        MultiAgentVersion::Disabled => false,
        MultiAgentVersion::V1 => !crate::agent::exceeds_thread_spawn_depth_limit(
            crate::agent::next_thread_spawn_depth(&turn_context.session_source),
            turn_context.config.agent_max_depth,
        ),
        MultiAgentVersion::V2 => crate::session::multi_agents::spawn_is_authorized(turn_context),
    };
    let control = &sess.services.agent_control;
    agent_surface_stage_from_snapshot(
        spawn_eligible,
        control.has_live_agents(),
        control.task_coordinator().has_bindings(),
    )
}

fn agent_surface_stage_from_snapshot(
    spawn_eligible: bool,
    child_graph_nonempty: bool,
    typed_bindings_present: bool,
) -> AgentSurfaceStage {
    if typed_bindings_present {
        AgentSurfaceStage::TypedAdministration
    } else if child_graph_nonempty {
        AgentSurfaceStage::Lifecycle
    } else if spawn_eligible {
        AgentSurfaceStage::SpawnOnly
    } else {
        AgentSurfaceStage::Prohibited
    }
}

fn goal_surface_state(
    executors: &[Arc<dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>>],
) -> GoalSurfaceState {
    if executors.iter().any(|executor| {
        matches!(
            executor.tool_name().name.as_str(),
            "get_goal" | "update_goal"
        ) && executor.exposure() == codex_tools::ToolExposure::Direct
    }) {
        GoalSurfaceState::Active
    } else if executors
        .iter()
        .any(|executor| executor.tool_name() == ToolName::plain("create_goal"))
    {
        GoalSurfaceState::Inactive
    } else {
        GoalSurfaceState::Disabled
    }
}

#[derive(Debug)]
struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
    settled_state: SamplingRequestSettledState,
    tool_result_continuation: bool,
    server_end_turn_false: bool,
}

#[derive(Debug)]
struct UnsettledSamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
    tool_result_continuation: bool,
    server_end_turn_false: bool,
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
    /// Raw citation payloads already surfaced through live delta events.
    emitted_memory_citations: HashSet<String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: HashSet::new(),
            leading_whitespace_by_item: HashMap::new(),
            emitted_memory_citations: HashSet::new(),
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

    async fn push_delta(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        delta: &str,
        memory_citation: Option<codex_protocol::memory_citation::MemoryCitation>,
    ) {
        if self.completed {
            return;
        }
        if delta.is_empty() && memory_citation.is_none() {
            return;
        }
        let event = PlanDeltaEvent {
            thread_id: sess.thread_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
            memory_citation,
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
    ) {
        if self.completed || !self.started {
            return;
        }
        self.completed = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        sess.emit_turn_item_completed(turn_context, item).await;
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

fn take_new_memory_citation(
    state: &mut PlanModeStreamState,
    citations: Vec<String>,
) -> Option<codex_protocol::memory_citation::MemoryCitation> {
    let citations = citations
        .into_iter()
        .filter(|citation| state.emitted_memory_citations.insert(citation.clone()))
        .collect();
    parse_memory_citation(citations)
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
        | EventMsg::TurnTerminalizationComplete(_)
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
        | EventMsg::ReasoningPolicyUpdated(_)
        | EventMsg::ReasoningPolicySummary(_)
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
                    memory_citation: None,
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
                        .push_delta(sess, turn_context, &delta, None)
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
    if let Some(state) = plan_mode_state {
        if let Some(memory_citation) = take_new_memory_citation(state, parsed.citations) {
            maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;
            let event = AgentMessageContentDeltaEvent {
                thread_id: sess.thread_id.to_string(),
                turn_id: turn_context.sub_id.clone(),
                item_id: item_id.to_string(),
                delta: String::new(),
                memory_citation: Some(memory_citation),
            };
            sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                .await;
        }
        if !parsed.plan_segments.is_empty() {
            handle_plan_segments(sess, turn_context, state, item_id, parsed.plan_segments).await;
        }
        return;
    }
    let memory_citation = parse_memory_citation(parsed.citations);
    if parsed.visible_text.is_empty() && memory_citation.is_none() {
        return;
    }
    let event = AgentMessageContentDeltaEvent {
        thread_id: sess.thread_id.to_string(),
        turn_id: turn_context.sub_id.clone(),
        item_id: item_id.to_string(),
        delta: parsed.visible_text,
        memory_citation,
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
) {
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
            let (plan_text, citations) = strip_citations(&plan_text);
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            if let Some(memory_citation) = take_new_memory_citation(state, citations) {
                state
                    .plan_item_state
                    .push_delta(sess, turn_context, "", Some(memory_citation))
                    .await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text)
                .await;
        }
    }
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
) {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        state.started_agent_message_items.remove(&agent_message_id);
        return;
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

    sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
        .await;
    state.started_agent_message_items.remove(&agent_message_id);
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
) {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(sess, turn_context, agent_message, state).await;
        }
        _ => {
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            sess.emit_turn_item_completed(turn_context, turn_item).await;
        }
    }
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_store: &codex_extension_api::ExtensionData,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
) -> bool {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        maybe_complete_plan_item_from_message(sess, turn_context, state, item).await;

        let mut finalized_facts = None;
        if let Some(finalized_turn_item) = finalize_non_tool_response_item(
            sess,
            TurnItemContributorPolicy::Run(turn_store),
            item,
            /*plan_mode*/ true,
        )
        .await
        {
            finalized_facts = Some(finalized_turn_item.facts.clone());
            emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                finalized_turn_item.turn_item,
                previously_active_item,
                state,
            )
            .await;
        }
        let final_last_agent_message = finalized_facts
            .as_ref()
            .and_then(|facts| facts.last_agent_message.clone());

        record_completed_response_item_with_finalized_facts(
            sess,
            turn_context,
            item,
            finalized_facts.as_ref(),
        )
        .await;
        if let Some(agent_message) = final_last_agent_message {
            *last_agent_message = Some(agent_message);
        }
        return true;
    }
    false
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
                let interrupt_terminal = sess
                    .active_turn
                    .lock()
                    .await
                    .as_ref()
                    .and_then(|active| active.terminal.clone())
                    .filter(|terminal| terminal.interrupt_pending());
                if let Some(terminal) = interrupt_terminal {
                    if let Err(err) = sess
                        .record_conversation_items_durable(
                            &turn_context,
                            std::slice::from_ref(&response_item),
                        )
                        .await
                    {
                        terminal.mark_interrupt_persistence_failed();
                        return Err(CodexErr::Fatal(format!(
                            "failed to durably append interrupted tool output: {err}"
                        )));
                    }
                } else {
                    sess.record_conversation_items(
                        &turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
                }
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

fn start_eager_tool_future(
    future: BoxFuture<'static, CodexResult<ResponseInputItem>>,
) -> BoxFuture<'static, CodexResult<ResponseInputItem>> {
    // Dropping a raw Tokio JoinHandle detaches its task. Keeping the abort-on-drop
    // wrapper inside the ordered future makes collection teardown abort eager work.
    let handle = AbortOnDropHandle::new(tokio::spawn(future));
    Box::pin(async move {
        handle
            .await
            .map_err(|err| CodexErr::Fatal(format!("eager tool task failed: {err}")))?
    })
}

fn assign_missing_streamed_response_item_id(
    item: &mut ResponseItem,
    active_item: Option<&TurnItem>,
) {
    if item.id().is_some_and(|id| !id.is_empty()) {
        return;
    }

    let active_item_id = active_item
        .map(|item| ResponseItemId::from_server(item.id()))
        .filter(|item_id| !item_id.is_empty());
    item.set_id(active_item_id);
    Session::assign_missing_response_item_id(item);
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
    generation_id: &ModelGenerationId,
    preparation_timing_guard: &mut Option<TurnTimingGuard>,
    reasoning_phase: Option<SamplingReasoningPhase>,
    reasoning_trigger: codex_protocol::protocol::ReasoningPolicyTrigger,
    sampling: SamplingGenerationDisposition,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    if sess.reference_context_item().await.is_none() {
        client_session.invalidate_provider_history_inheritance(
            "realized context baseline is unknown before sampling",
        );
    }
    let terminal = {
        let active_turn = sess.active_turn.lock().await;
        active_turn
            .as_ref()
            .and_then(|active| active.terminal.clone())
    };
    if let Some(terminal) = terminal
        && terminal.sampling_admission() == SamplingAdmission::Fenced
    {
        terminal.wait_for_interrupt_resolution().await;
        if terminal.interrupt_persistence_failed() {
            return Err(CodexErr::Fatal(
                "interrupted request_user_input output was not durably persisted".to_string(),
            ));
        }
        return Err(CodexErr::TurnAborted);
    }
    let request_policy = crate::session::reasoning_governor::resolve_request_policy_for_generation(
        reasoning_phase,
        turn_context.config.reasoning_phase_efforts.as_ref(),
        turn_context.configured_reasoning_effort.clone(),
        &turn_context.model_info,
        &sampling,
    );
    feedback_tags!(
        model = turn_context.model_info.slug.clone(),
        approval_policy = turn_context.approval_policy.value(),
        sandbox_policy = &turn_context.sandbox_policy(),
        effort = request_policy.request_effort,
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
    client_session.set_turn_timing(Arc::clone(&turn_context.turn_timing_state));
    if let Some(startup_prewarm) = sess.take_session_startup_prewarm().await {
        let claim_started_at = std::time::Instant::now();
        let startup_wait = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::StartupPrewarmWait);
        let resolution = startup_prewarm
            .resolve(
                &turn_context.session_telemetry,
                &sess.startup_timing,
                &cancellation_token,
            )
            .await;
        drop(startup_wait);
        let (timing_status, claim_status) = match resolution {
            SessionStartupPrewarmResolution::Cancelled => return Err(CodexErr::TurnAborted),
            SessionStartupPrewarmResolution::Ready(prewarmed_session) => {
                turn_context
                    .turn_timing_state
                    .record_ready_startup_prewarm();
                let claim = client_session
                    .claim_startup_prewarm(
                        *prewarmed_session,
                        prompt,
                        &turn_context.model_info,
                        request_policy.request_effort.clone(),
                        turn_context.reasoning_summary,
                        turn_context.config.service_tier.clone(),
                        responses_metadata,
                    )
                    .await?;
                match claim {
                    StartupPrewarmClaim::ResponseChain => {
                        ("prewarm_winner", "ready_compatible_reuse")
                    }
                    StartupPrewarmClaim::TransportOnly => {
                        ("prewarm_transport_winner", "ready_transport_reuse")
                    }
                    StartupPrewarmClaim::Rejected => ("stale_incompatible", "stale_incompatible"),
                }
            }
            SessionStartupPrewarmResolution::Unavailable { .. } => {
                ("ordinary_dispatch_winner", "ordinary_dispatch_winner")
            }
        };
        sess.startup_timing.record_prewarm_status(timing_status);
        turn_context.session_telemetry.record_startup_phase(
            "startup_prewarm_claim",
            claim_started_at.elapsed(),
            Some(claim_status),
        );
    }
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
    if let Some(recorder) = sess.active_reasoning_policy_recorder().await
        && let Some(snapshot) = recorder.append(
            &request_policy,
            turn_context.model_info.slug.clone(),
            reasoning_trigger,
        )
    {
        sess.try_send_live_event(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::ReasoningPolicyUpdated(snapshot),
        });
    }
    let boundary_session = Arc::clone(&sess);
    let boundary_turn_id = turn_context.sub_id.clone();
    let attempt_prepared: AttemptPreparedCallback = Arc::new(move |identity| {
        let sess = Arc::clone(&boundary_session);
        let turn_id = boundary_turn_id.clone();
        Box::pin(async move {
            let terminal = sess
                .active_turn
                .lock()
                .await
                .as_ref()
                .and_then(|active| active.terminal.clone());
            let sampling_admission = if let Some(terminal) = terminal.as_ref() {
                let Some(admission) = terminal.acquire_sampling_admission().await else {
                    terminal.wait_for_interrupt_resolution().await;
                    return if terminal.interrupt_persistence_failed() {
                        Err(CodexErr::Fatal(
                            "interrupted request_user_input output was not durably persisted"
                                .to_string(),
                        ))
                    } else {
                        Err(CodexErr::TurnAborted)
                    };
                };
                Some(admission)
            } else {
                None
            };
            sess.persist_rollout_items_durable(&[RolloutItem::SamplingBoundary(
                SamplingBoundaryItem {
                    sampling_request_id: identity.sampling_request_id.clone(),
                    physical_attempt_id: identity.physical_attempt_id.clone(),
                    turn_id: Some(turn_id),
                    unresolved_context: true,
                },
            )])
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to persist sampling boundary before provider dispatch: {err}"
                ))
            })?;
            sess.bind_context_baseline_candidate(
                &identity.sampling_request_id,
                &identity.physical_attempt_id,
            )
            .await;
            Ok(sampling_admission)
        })
    });
    let stream_result = client_session
        .stream_with_attempt_prepared(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            request_policy.request_effort.clone(),
            turn_context.reasoning_summary,
            turn_context.config.service_tier.clone(),
            responses_metadata,
            &inference_trace,
            Some(attempt_prepared),
        )
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await;
    drop(model_request_timing_guard);
    let mut stream = stream_result??;
    let attempt_identity = stream.attempt_identity().cloned();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    let mut earlier_tool_calls_eligible = true;
    let mut needs_follow_up = false;
    let mut tool_result_continuation = false;
    let mut server_end_turn_false = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut active_tool_argument_diff_consumer: Option<(
        String,
        Box<dyn ToolArgumentDiffConsumer>,
    )> = None;
    let mut should_emit_turn_diff = false;
    let mut should_emit_token_count = false;
    let mut latest_models_etag = None;
    let reasoning_effort = request_policy
        .request_effort
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "default".to_string());
    let reasoning_phase = request_policy
        .phase
        .map(|phase| match phase {
            SamplingReasoningPhase::Orient => "orient",
            SamplingReasoningPhase::Inspect => "inspect",
            SamplingReasoningPhase::Implement => "implement",
            SamplingReasoningPhase::Verify => "verify",
            SamplingReasoningPhase::Diagnose => "diagnose",
            SamplingReasoningPhase::Finalize => "finalize",
        })
        .unwrap_or("disabled");
    let reasoning_effort_source = match request_policy.source {
        codex_protocol::protocol::ReasoningPolicySource::PhaseOverride => "phase_override",
        codex_protocol::protocol::ReasoningPolicySource::TurnFallback => "turn_fallback",
    };
    let plan_mode = turn_context.collaboration_mode.mode == ModeKind::Plan;
    let mut assistant_message_stream_parsers = AssistantMessageStreamParsers::new(plan_mode);
    let mut plan_mode_state = plan_mode.then(|| PlanModeStreamState::new(&turn_context.sub_id));
    let defer_streamed_turn_items_for_contributors =
        !sess.services.extensions.turn_item_contributors().is_empty();
    let mut active_item_is_streaming_to_client = false;
    let receiving_span = trace_span!("receiving_stream");
    let outcome: CodexResult<UnsettledSamplingRequestResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
            codex.request.reasoning_effort = %reasoning_effort,
            codex.request.reasoning_phase = reasoning_phase,
            codex.request.reasoning_effort_source = reasoning_effort_source,
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

        match event {
            ResponseEvent::Created => {
                if let Some(identity) = attempt_identity.as_ref()
                    && let Err(err) = sess
                        .commit_context_baseline_candidate(
                            &identity.sampling_request_id,
                            &identity.physical_attempt_id,
                        )
                        .await
                {
                    sess.mark_context_baseline_unknown().await;
                    client_session.invalidate_provider_history_inheritance(
                        "authoritative context persistence failed after provider acceptance",
                    );
                    break Err(CodexErr::Fatal(format!(
                        "provider accepted sampling attempt but authoritative context persistence failed: {err}"
                    )));
                }
            }
            ResponseEvent::OutputItemDone(mut item) => {
                if turn_context.item_ids_enabled() {
                    assign_missing_streamed_response_item_id(&mut item, active_item.as_ref());
                }
                if let Some((_, mut consumer)) = active_tool_argument_diff_consumer.take()
                    && let Ok(Some(event)) = consumer.finish()
                {
                    sess.send_event(&turn_context, event).await;
                }
                let previously_active_item = active_item.take();
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
                if let Some(state) = plan_mode_state.as_mut()
                    && handle_assistant_item_done_in_plan_mode(
                        &sess,
                        &turn_context,
                        turn_store.as_ref(),
                        &item,
                        state,
                        previously_streamed_item.as_ref(),
                        &mut last_agent_message,
                    )
                    .await
                {
                    continue;
                }

                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    turn_store: Arc::clone(&turn_store),
                    tool_runtime: tool_runtime.clone(),
                    cancellation_token: cancellation_token.child_token(),
                };

                let output_result = match handle_output_item_done(
                    &mut ctx,
                    item,
                    previously_streamed_item,
                    &mut earlier_tool_calls_eligible,
                )
                .instrument(handle_responses)
                .await
                {
                    Ok(output_result) => output_result,
                    Err(err) => break Err(err),
                };
                if let Some(tool_future) = output_result.tool_future {
                    if output_result.eager_read_eligible {
                        in_flight.push_back(start_eager_tool_future(tool_future));
                    } else {
                        in_flight.push_back(tool_future);
                    }
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
                tool_result_continuation |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(mut item) => {
                if turn_context.item_ids_enabled() {
                    assign_missing_streamed_response_item_id(&mut item, /*active_item*/ None);
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
                    let stream_item_to_client = !defer_streamed_turn_items_for_contributors;
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
                sess.mark_tool_history_consumed(
                    &turn_context,
                    &prompt.input,
                    generation_id.clone(),
                )
                .await;
                turn_context
                    .turn_timing_state
                    .record_generation_token_usage(token_usage.as_ref());
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
                    server_end_turn_false = true;
                }
                break Ok(UnsettledSamplingRequestResult {
                    needs_follow_up,
                    last_agent_message,
                    tool_result_continuation,
                    server_end_turn_false,
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
                            memory_citation: None,
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
    drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;
    drop(tool_blocking_timing_guard);

    let terminal = {
        let active_turn = sess.active_turn.lock().await;
        active_turn
            .as_ref()
            .and_then(|active| active.terminal.clone())
    };
    if let Some(terminal) = terminal
        && terminal.interrupt_pending()
    {
        if let Err(err) = sess.flush_rollout().await {
            terminal.mark_interrupt_persistence_failed();
            return Err(CodexErr::Fatal(format!(
                "failed to durably flush interrupted request_user_input output: {err}"
            )));
        }
        terminal.mark_interrupt_output_durable();
        return Err(CodexErr::TurnAborted);
    }

    // A tool result guarantees another request in this turn. A later assistant
    // item in the same response must not defer already queued mailbox input.
    if tool_result_continuation {
        sess.input_queue
            .accept_mailbox_delivery_for_current_turn(&sess.active_turn, &turn_context.sub_id)
            .await;
    }

    if should_emit_token_count {
        // A tool call such as request_user_input can intentionally pause the turn. Emit token
        // counts only after pending tools resolve so clients do not see progress events while the
        // turn is waiting on the user. This also needs to happen before returning cancellation so
        // token usage already recorded from the completed response is still persisted.
        sess.send_token_count_event(&turn_context).await;
    }

    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }

    let settled_state = {
        let tracker = turn_diff_tracker.lock().await;
        SamplingRequestSettledState {
            mutation_revision: tracker.current_mutation_revision(),
            validation_status: tracker.validation_freshness_status(),
            validation_revision: tracker.last_successful_validation_revision(),
        }
    };
    let outcome = outcome.map(|result| SamplingRequestResult {
        needs_follow_up: result.needs_follow_up,
        last_agent_message: result.last_agent_message,
        settled_state,
        tool_result_continuation: result.tool_result_continuation,
        server_end_turn_false: result.server_end_turn_false,
    });

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

    if let Some(etag) = latest_models_etag {
        sess.services
            .models_manager
            .clone()
            .notify_etag(etag, turn_context.config.http_client_factory())
            .await;
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
