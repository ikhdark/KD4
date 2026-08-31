use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use crate::agents_md::RepositoryStableContextBundle;
use crate::apply_skill_injection_observability;
use crate::client::AttemptPreparedCallback;
use crate::client::ModelClientSession;
use crate::client::StartupPrewarmClaim;
use crate::client_common::Prompt;
use crate::client_common::PromptDigests;
use crate::client_common::ResponseEvent;
use crate::client_common::ToolSchemaArtifact;
use crate::collect_explicit_skill_mentions;
use crate::compact::InitialContextInjection;
use crate::compact::InlineAutoCompactReuse;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote_v2::run_inline_remote_auto_compact_task as run_inline_remote_auto_compact_task_v2;
use crate::connectors;
use crate::context::ApprovalPromptContext;
use crate::context::ContextualUserFragment;
use crate::context::PermissionsInstructions;
use crate::context::PromptContextCategory;
use crate::context::RecommendedPluginsInstructions;
use crate::context::TaskModelGuidance;
use crate::context::base_instructions_own_task_model_guidance;
use crate::context::is_startup_contextual_user_fragment;
use crate::context_manager::ContextManager;
use crate::context_manager::PreparedPromptInput;
use crate::context_manager::compact_acknowledged_tool_search_outputs;
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
use crate::invariants::error_or_panic;
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
use crate::plan_skill_injections;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::build_plugin_injections;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::session::EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
use crate::session::PreparedContextUpdate;
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
use crate::stable_context::filter_unchanged_stable_context_items;
use crate::stable_context::mark_trusted_stable_context_item;
use crate::state::SamplingAdmission;
use crate::state::TerminalWakeResult;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::InFlightToolCall;
use crate::stream_events_utils::InFlightToolResult;
use crate::stream_events_utils::OrderedResponseItemRecorder;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::stream_events_utils::mark_thread_memory_mode_polluted_if_external_context;
use crate::stream_events_utils::raw_assistant_output_text_from_item;
use crate::stream_events_utils::record_completed_response_item_with_finalized_facts;
use crate::tasks::TurnTaskResult;
use crate::tasks::emit_compact_metric;
use crate::tool_history::ModelGenerationId;
use crate::tools::ToolRouter;
use crate::tools::context::RequiredToolTerminal;
use crate::tools::context::RequiredToolTerminalCause;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::DirectMcpToolEntrypoint;
use crate::tools::exposure::DynamicToolExposureIdentity;
use crate::tools::exposure::EnvironmentSurfaceMode;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolRouterParams;
use crate::tools::router::ToolSuggestCandidates;
use crate::tools::router::ToolSuggestPresentation;
use crate::tools::router::extension_tool_executors;
use crate::tools::router::extension_tool_surface_revision;
use crate::tools::spec_plan::search_tool_enabled;
use crate::tools::spec_plan::tool_suggest_enabled;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::ContinuationCause;
use crate::turn_timing::TurnLocalPhase;
use crate::turn_timing::TurnTimingGuard;
use crate::turn_timing::TurnTimingState;
use crate::turn_timing::record_turn_ttft_metric;
use codex_analytics::AppInvocation;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::InvocationType;
use codex_analytics::SkillInvocation;
use codex_analytics::TrackEventsContext;
use codex_analytics::TurnResolvedConfigFact;
use codex_analytics::TurnSubmissionType;
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
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentReasoningSectionBreakEvent;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::NextSampleBlockReason;
use codex_protocol::protocol::PlanDeltaEvent;
use codex_protocol::protocol::ReasoningContentDeltaEvent;
use codex_protocol::protocol::ReasoningRawContentDeltaEvent;
use codex_protocol::protocol::SafetyBufferingEvent;
use codex_protocol::protocol::SamplingBoundaryItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SurfacedToolResult;
use codex_protocol::protocol::ToolLifecycleTimerWait;
use codex_protocol::protocol::ToolLifecycleWakeReason;
use codex_protocol::protocol::TurnDiffEvent;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_tools::DiscoverableTool;
use codex_tools::ToolName;
use codex_tools::filter_request_plugin_install_discoverable_tools_for_client;
use codex_tools::request_user_input_available_modes;
use codex_utils_stream_parser::AssistantTextChunk;
use codex_utils_stream_parser::AssistantTextStreamParser;
use codex_utils_stream_parser::ProposedPlanSegment;
use codex_utils_stream_parser::extract_proposed_plan_text;
use codex_utils_stream_parser::strip_citations;
use futures::future::BoxFuture;
use futures::future::Either;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
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
    let workspace_identity = if history.requires_workspace_evidence_validation() {
        git_workspace
            .workspace_evidence_identity(turn_context.config.cwd.as_path())
            .await
    } else {
        None
    };
    prepare_sampling_prompt_with_workspace_identity(
        history,
        turn_context,
        workspace_identity.as_ref(),
        git_workspace,
    )
}

fn prepare_sampling_prompt_with_workspace_identity(
    history: ContextManager,
    turn_context: &TurnContext,
    workspace_identity: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
    git_workspace: &crate::git_workspace::GitWorkspaceCache,
) -> PreparedPromptInput {
    if turn_context.config.completed_tool_history_projection {
        history.prepare_for_sampling_prompt_with_completed_tool_projection(
            &turn_context.model_info.input_modalities,
            StableContextTarget::Sampling,
            workspace_identity,
            git_workspace,
        )
    } else {
        history.prepare_for_sampling_prompt_with_workspace_freshness(
            &turn_context.model_info.input_modalities,
            StableContextTarget::Sampling,
            workspace_identity,
            git_workspace,
        )
    }
}

fn continuation_workspace_prefetch_is_current(
    baseline_mutation_revision: u64,
    current_mutation_revision: u64,
    accepted_user_input: bool,
) -> bool {
    baseline_mutation_revision == current_mutation_revision && !accepted_user_input
}

async fn start_continuation_workspace_prefetch(
    history: &ContextManager,
    turn_diff_tracker: &Arc<tokio::sync::Mutex<TurnDiffTracker>>,
    git_workspace: Arc<crate::git_workspace::GitWorkspaceCache>,
    cwd: codex_utils_absolute_path::AbsolutePathBuf,
) -> Option<(
    u64,
    AbortOnDropHandle<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
)> {
    if !history.requires_workspace_evidence_validation() {
        return None;
    }
    let baseline_mutation_revision = turn_diff_tracker.lock().await.current_mutation_revision();
    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        git_workspace
            .workspace_evidence_identity(cwd.as_path())
            .await
    }));
    Some((baseline_mutation_revision, handle))
}

async fn finish_stopped_session_start(sess: &Session, input: Vec<TurnInput>) -> TurnTaskResult {
    let defer_pending_input = !input.is_empty();
    sess.input_queue
        .restore_transferred_startup_input(input)
        .await;
    TurnTaskResult {
        defer_pending_input,
        ..TurnTaskResult::default()
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
    let mut mutating_finalizer_ran = false;
    let mut preparation_timing_guard = None;
    let mut client_session =
        prewarmed_client_session.unwrap_or_else(|| sess.services.model_client.new_session());
    if !sess.has_reference_context_item().await {
        client_session
            .invalidate_provider_history_inheritance("realized context baseline is unknown");
    }
    let planning_timing_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::Planning);
    let mut planning_iterations = 0;
    let mut completed_mcp_effect = None;
    let pending_turn_plan_result = loop {
        let mut pending_turn_plan = match stabilize_pending_turn_plan(
            &sess,
            &turn_context,
            &input,
            &mut client_session,
            &mut planning_iterations,
            &mut completed_mcp_effect,
            &cancellation_token,
        )
        .await
        {
            Ok(plan) => plan,
            Err(err) => break Err(err),
        };
        let prepared_context_update =
            take_stabilized_context_update(&mut pending_turn_plan.prepared_context_update)?;
        let (world_state, display_roots) = tokio::join!(
            sess.compare_and_record_context_updates(
                prepared_context_update,
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
            return finish_pending_turn_planning_failure(&sess, &turn_context, &input, err).await;
        }
    };
    commit_pending_turn_plan_effects(&sess, &turn_context, &pending_turn_plan).await;
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
        return Ok(finish_stopped_session_start(&sess, input).await);
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
    }))
    .await;
    if !injection_items.is_empty() {
        sess.record_conversation_items(&turn_context, &injection_items)
            .await;
    }

    track_turn_resolved_config_analytics(&sess, &turn_context, &input).await;

    let mut last_agent_message: Option<String> = None;
    let mut surfaced_result: Option<SurfacedToolResult> = None;
    let mut stop_hook_active = false;
    let mut pending_continuation_cause = None;
    let mut pending_generation_request: Option<GenerationRequestDisposition> = None;
    let mut prefetched_workspace_identity: Option<
        Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    > = None;
    let mut has_started_generation = false;
    let mut logical_generation_ordinal = 0_u32;
    let mut logical_generation_budget = LogicalGenerationBudget::default();
    let mut generation_budget_error_reported = false;
    let mut defer_pending_input = false;
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
    // The turn owns its fully rendered instructions; the scaffold clones the
    // text only on a cache miss.
    let base_instructions = Arc::clone(&turn_context.base_instructions);
    'sampling_loop: loop {
        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        let Some(pending_input) =
            drain_pending_input_if_generation_available(&logical_generation_budget, async {
                if can_drain_pending_input {
                    sess.input_queue.get_pending_input(&sess.active_turn).await
                } else {
                    Vec::new()
                }
            })
            .await
        else {
            defer_pending_input = true;
            report_logical_generation_budget_exhausted(
                sess.as_ref(),
                turn_context.as_ref(),
                &mut generation_budget_error_reported,
            )
            .await;
            break 'sampling_loop;
        };

        let recorded_input =
            run_hooks_and_record_inputs_detailed(&sess, &turn_context, &pending_input).await;
        if recorded_input.accepted_context_input {
            prefetched_workspace_identity = None;
            mutating_finalizer_ran = false;
            reasoning_governor.accepted_user_input();
        }
        if recorded_input.should_stop {
            emit_status_affecting_turn_error(
                sess.as_ref(),
                turn_context.as_ref(),
                "The UserPromptSubmit hook stopped the turn before a model response was produced.",
            )
            .await;
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
            reasoning_governor.baselines_with_tool_exposure_revision(
                tracker.current_mutation_revision(),
                turn_context.deferred_tool_activation_revision(),
            )
        };
        let request_signals = reasoning_governor.collector(&request_baselines);
        let mut generation_request = pending_generation_request.take().unwrap_or_else(|| {
            if !has_started_generation {
                reasoning_governor.initial_generation_request(&request_baselines)
            } else {
                GenerationRequestDisposition {
                    purpose: match pending_continuation_cause {
                        Some(ContinuationCause::Compaction) => {
                            Some(TurnTimingGenerationPurpose::CompactionRecovery)
                        }
                        Some(ContinuationCause::InvalidImageRecovery)
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
        if matches!(
            pending_continuation_cause,
            Some(ContinuationCause::Compaction)
        ) {
            generation_request = rebase_generation_request_after_compaction(
                generation_request,
                request_baselines.relevant_state_fingerprint(),
            );
        }
        let generation_budget_admission =
            logical_generation_budget.admit(generation_request.terminal_completion_only);
        let budget_forced_terminal = match generation_budget_admission {
            LogicalGenerationAdmission::Regular => false,
            LogicalGenerationAdmission::Terminal { forced } => {
                generation_request = generation_request.require_terminal_completion();
                forced
            }
            LogicalGenerationAdmission::Exhausted => {
                defer_pending_input = true;
                report_logical_generation_budget_exhausted(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &mut generation_budget_error_reported,
                )
                .await;
                break 'sampling_loop;
            }
        };
        if budget_forced_terminal {
            record_forced_terminal_budget_boundary(sess.as_ref(), turn_context.as_ref()).await;
        }
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

            // Request preparation begins with the history snapshot. Pending-turn
            // planning, hooks, input recording, and analytics above remain owned
            // orchestration, but are not part of the model-request preparation lane.
            // The guard is consumed at dispatch, so begin it again for every
            // logical generation rather than measuring only the first request.
            turn_context
                .turn_timing_state
                .begin_request_preparation(&mut preparation_timing_guard);

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
                let prepared = match prefetched_workspace_identity.take() {
                    Some(workspace_identity) => prepare_sampling_prompt_with_workspace_identity(
                        history,
                        turn_context.as_ref(),
                        workspace_identity.as_ref(),
                        sess.services.git_workspace.as_ref(),
                    ),
                    None => {
                        prepare_sampling_prompt_for_client(
                            history,
                            turn_context.as_ref(),
                            &client_session,
                            sess.services.git_workspace.as_ref(),
                        )
                        .await
                    }
                };
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
                &base_instructions,
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
        // Dispatch normally consumes this guard. A request that fails before
        // dispatch must close it here so error lifecycle and cancellation work
        // remain orchestration rather than request preparation.
        turn_context
            .turn_timing_state
            .finish_request_preparation(&mut preparation_timing_guard);
        match sampling_request_result {
            Ok((sampling_request_output, sampling_request_input)) => {
                let SamplingRequestResult {
                    needs_follow_up: model_needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                    settled_state,
                    tool_result_continuation,
                    server_end_turn_false,
                    required_tool_terminal,
                    prefetched_workspace_identity: next_workspace_identity,
                } = sampling_request_output;
                prefetched_workspace_identity = next_workspace_identity;
                if let Some(required_tool_terminal) = required_tool_terminal {
                    if required_tool_terminal.cause != RequiredToolTerminalCause::Blocked {
                        let error = CodexErrorInfo::Other;
                        sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                            .await;
                        sess.send_event(
                            &turn_context,
                            EventMsg::Error(ErrorEvent {
                                message: required_tool_terminal.message.clone(),
                                codex_error_info: Some(error),
                            }),
                        )
                        .await;
                    }
                    return Ok(TurnTaskResult {
                        last_agent_message,
                        surfaced_result,
                        required_tool_terminal: Some(required_tool_terminal),
                        defer_pending_input: false,
                    });
                }
                reasoning_governor.settle(&request_baselines, &request_signals, &settled_state);
                can_drain_pending_input = true;
                let (has_pending_input, token_status) = collect_post_sampling_state(
                    sess.input_queue.has_pending_input(&sess.active_turn),
                    super::context_window::context_window_token_status(
                        sess.as_ref(),
                        turn_context.as_ref(),
                    ),
                )
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
                if budget_forced_terminal {
                    // The budget grants one final tool-free synthesis request.
                    // Queued input remains in the mailbox for the next turn;
                    // it must not reopen this exhausted sampling loop.
                    defer_pending_input = true;
                    needs_follow_up = false;
                }
                let progress_kinds =
                    request_signals.progress_kinds(&request_baselines, &settled_state);
                let mut convergence_decision = if needs_follow_up && !has_pending_input {
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
                let server_resample_eligible =
                    server_end_turn_false && !tool_result_continuation && !has_pending_input;
                let mut next_generation_request = needs_follow_up.then(|| {
                    reasoning_governor.continuation_generation_request(
                        &request_baselines,
                        &request_signals,
                        &settled_state,
                        has_pending_input,
                        protocol_resample_completion_allowed(server_resample_eligible),
                    )
                });
                if next_generation_request
                    .as_ref()
                    .is_some_and(|request| request.sampling.is_residual_deterministic())
                {
                    turn_context
                        .turn_timing_state
                        .record_residual_deterministic_generation();
                }
                if terminal_completion_required {
                    next_generation_request = next_generation_request
                        .map(GenerationRequestDisposition::require_terminal_completion);
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
                if needs_follow_up
                    && generation_budget_blocks_follow_up(
                        &logical_generation_budget,
                        next_generation_request.as_ref(),
                    )
                {
                    defer_pending_input = true;
                    report_logical_generation_budget_exhausted(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &mut generation_budget_error_reported,
                    )
                    .await;
                    needs_follow_up = false;
                    next_generation_request = None;
                }
                turn_context.turn_timing_state.record_generation_outcome(
                    progress_kinds.clone(),
                    generation_request_action_changed(
                        &generation_request,
                        next_generation_request.as_ref(),
                    ),
                    progress_kinds.is_empty()
                        && !request_signals.observed_successful_process_monitor(),
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
                    record_convergence_decision(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        convergence_decision.as_mut(),
                    )
                    .await;
                    if let Err(err) = run_auto_compact(
                        &sess,
                        Arc::clone(&step_context),
                        /*fallback_step_context*/ None,
                        &mut client_session,
                        prefetched_workspace_identity.as_ref(),
                        InitialContextInjection::AtStart(Arc::clone(&world_state)),
                        CompactionReason::ContextLimit,
                        CompactionPhase::MidTurn,
                        &cancellation_token,
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
                    // C1: the direct runtime shares this completion path. It must not skip
                    // the after-agent and completion-stop hooks, because those are the
                    // authoritative, user-configured control over whether a turn may finish.
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
                        after_agent_outcome.aborted
                    } else {
                        false
                    };
                    let completion_stop_report = run_completion_stop_hook(
                        &sess,
                        &turn_context,
                        stop_hook_active,
                        last_agent_message.clone(),
                    )
                    .await;
                    if let Some(hook_prompt_message) = completion_stop_report.continuation_prompt
                        && admit_regular_follow_up(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            &logical_generation_budget,
                            &mut generation_budget_error_reported,
                        )
                        .await
                    {
                        sess.record_response_item_and_emit_turn_item(
                            &turn_context,
                            hook_prompt_message,
                        )
                        .await;
                        stop_hook_active = true;
                        reasoning_governor.host_diagnose();
                        clear_pending_generation_request(&mut pending_generation_request);
                        pending_continuation_cause = Some(ContinuationCause::StopHook);
                        continue 'sampling_loop;
                    }
                    if completion_stop_report.should_stop {
                        if mutating_finalizer_aborted {
                            return Ok(after_agent_abort_result(
                                last_agent_message,
                                surfaced_result,
                                defer_pending_input,
                            ));
                        }
                        emit_hook_stop_reason(
                            &sess,
                            &turn_context,
                            "Stop",
                            completion_stop_report.stop_reason.as_deref(),
                        )
                        .await;
                        break 'sampling_loop;
                    }
                    let after_agent_aborted = if matches!(
                        turn_context.config.after_agent_policy,
                        AfterAgentPolicy::Legacy
                    ) {
                        run_legacy_after_agent_hook(
                            &sess,
                            &turn_context,
                            &sampling_request_input,
                            last_agent_message.clone(),
                        )
                        .await
                        .aborted
                    } else {
                        false
                    };
                    if mutating_finalizer_aborted || after_agent_aborted {
                        return Ok(after_agent_abort_result(
                            last_agent_message,
                            surfaced_result,
                            defer_pending_input,
                        ));
                    }
                    match completion_pending_input_disposition(
                        &logical_generation_budget,
                        sess.input_queue.has_pending_input(&sess.active_turn).await,
                    ) {
                        CompletionPendingInputDisposition::Continue => {
                            pending_continuation_cause = Some(ContinuationCause::PendingInput);
                            continue 'sampling_loop;
                        }
                        CompletionPendingInputDisposition::Defer => {
                            defer_pending_input = true;
                        }
                        CompletionPendingInputDisposition::None => {}
                    }
                    break;
                }
                pending_continuation_cause = ordinary_continuation_cause(
                    tool_result_continuation,
                    server_end_turn_false,
                    has_pending_input,
                );
                record_convergence_decision(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    convergence_decision.as_mut(),
                )
                .await;
                debug_assert!(pending_continuation_cause.is_some());
                continue;
            }
            Err(err @ CodexErr::TurnAborted) => {
                return Err(err);
            }
            Err(codex_error @ CodexErr::InvalidImageRequest()) => {
                let sanitized = {
                    let mut state = sess.state.lock().await;
                    error_or_panic(
                        "Invalid image detected; sanitizing tool output to prevent poisoning",
                    );
                    state.history.replace_last_turn_images("Invalid image")
                };
                if sanitized {
                    if !admit_regular_follow_up(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &logical_generation_budget,
                        &mut generation_budget_error_reported,
                    )
                    .await
                    {
                        break;
                    } else {
                        clear_pending_generation_request(&mut pending_generation_request);
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
        required_tool_terminal: None,
        defer_pending_input,
    })
}

fn clear_pending_generation_request<Request>(pending_generation_request: &mut Option<Request>) {
    *pending_generation_request = None;
}

fn rebase_generation_request_after_compaction(
    mut request: GenerationRequestDisposition,
    relevant_state_fingerprint: String,
) -> GenerationRequestDisposition {
    request.purpose = Some(if request.terminal_completion_only {
        TurnTimingGenerationPurpose::TerminalCompletionReasoning
    } else {
        TurnTimingGenerationPurpose::CompactionRecovery
    });
    request.sampling = SamplingGenerationDisposition::DecisionBearing;
    request.relevant_state_fingerprint = relevant_state_fingerprint;
    request
}

fn after_agent_abort_result(
    last_agent_message: Option<String>,
    surfaced_result: Option<SurfacedToolResult>,
    defer_pending_input: bool,
) -> TurnTaskResult {
    TurnTaskResult {
        last_agent_message,
        surfaced_result,
        required_tool_terminal: None,
        defer_pending_input,
    }
}

async fn finish_pending_turn_planning_failure(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
    err: CodexErr,
) -> CodexResult<TurnTaskResult> {
    run_hooks_and_record_inputs(sess, turn_context, input).await;
    if matches!(err, CodexErr::TurnAborted) {
        return Err(err);
    }

    let error = err.to_codex_protocol_error();
    sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
        .await;
    sess.track_turn_codex_error(turn_context.as_ref(), &err);
    sess.send_event(
        turn_context.as_ref(),
        EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
    )
    .await;
    error!("Pending-turn planning failed before persistence or model send: {err}");
    Ok(TurnTaskResult::default())
}

async fn collect_post_sampling_state<A, B>(
    pending_input: impl Future<Output = A> + Send,
    token_status: impl Future<Output = B> + Send,
) -> (A, B) {
    tokio::join!(pending_input, token_status)
}

async fn join_recommendations_and_mcp<A, B>(
    recommendations: impl Future<Output = A> + Send,
    mcp_tools: impl Future<Output = B> + Send,
) -> (A, B) {
    tokio::join!(recommendations, mcp_tools)
}

async fn collect_projected_prompt_state<A, B, C>(
    active_context: impl Future<Output = A> + Send,
    committed_history: impl Future<Output = B> + Send,
    auto_compact_window: impl Future<Output = C> + Send,
) -> (A, B, C) {
    tokio::join!(active_context, committed_history, auto_compact_window)
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

// Keep a finite emergency boundary for genuinely non-converging turns while
// leaving room for legitimate multi-step tool work. Deterministic repeated
// cycles are handled earlier by the reasoning governor.
const MAX_REGULAR_LOGICAL_GENERATIONS: u32 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalGenerationAdmission {
    Regular,
    Terminal { forced: bool },
    Exhausted,
}

#[derive(Default)]
struct LogicalGenerationBudget {
    regular_generations: u32,
    terminal_generation_used: bool,
}

impl LogicalGenerationBudget {
    fn is_exhausted(&self) -> bool {
        self.regular_generations >= MAX_REGULAR_LOGICAL_GENERATIONS && self.terminal_generation_used
    }

    fn has_regular_generation_capacity(&self) -> bool {
        self.regular_generations < MAX_REGULAR_LOGICAL_GENERATIONS
    }

    fn admit(&mut self, terminal_requested: bool) -> LogicalGenerationAdmission {
        if terminal_requested {
            if self.terminal_generation_used {
                return LogicalGenerationAdmission::Exhausted;
            }
            self.terminal_generation_used = true;
            return LogicalGenerationAdmission::Terminal { forced: false };
        }
        if self.regular_generations < MAX_REGULAR_LOGICAL_GENERATIONS {
            self.regular_generations = self.regular_generations.saturating_add(1);
            return LogicalGenerationAdmission::Regular;
        }
        if self.terminal_generation_used {
            return LogicalGenerationAdmission::Exhausted;
        }
        self.terminal_generation_used = true;
        LogicalGenerationAdmission::Terminal { forced: true }
    }

    fn can_admit(&self, terminal_requested: bool) -> bool {
        if terminal_requested {
            !self.terminal_generation_used
        } else {
            self.regular_generations < MAX_REGULAR_LOGICAL_GENERATIONS
                || !self.terminal_generation_used
        }
    }
}

fn generation_budget_blocks_follow_up(
    budget: &LogicalGenerationBudget,
    next_generation_request: Option<&GenerationRequestDisposition>,
) -> bool {
    next_generation_request
        .is_some_and(|request| !budget.can_admit(request.terminal_completion_only))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionPendingInputDisposition {
    None,
    Continue,
    Defer,
}

fn completion_pending_input_disposition(
    budget: &LogicalGenerationBudget,
    has_pending_input: bool,
) -> CompletionPendingInputDisposition {
    if !has_pending_input {
        CompletionPendingInputDisposition::None
    } else if budget.has_regular_generation_capacity() {
        CompletionPendingInputDisposition::Continue
    } else {
        CompletionPendingInputDisposition::Defer
    }
}

const LOGICAL_GENERATION_BUDGET_EXHAUSTED_MESSAGE: &str =
    "The turn reached its logical generation limit before all requested work completed.";
const LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE: &str = "The logical generation limit has been reached. This is the final tool-free synthesis request. Do not call tools. Summarize completed work and truthfully report any remaining work, failed validation, or blocker.";
async fn record_forced_terminal_budget_boundary(sess: &Session, turn_context: &TurnContext) {
    let directive_item = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    sess.record_conversation_items(turn_context, std::slice::from_ref(&directive_item))
        .await;
    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE.to_string(),
        }),
    )
    .await;
}

async fn emit_status_affecting_turn_error(
    sess: &Session,
    turn_context: &TurnContext,
    message: impl Into<String>,
) {
    let error = CodexErrorInfo::Other;
    sess.emit_turn_error_lifecycle(turn_context, error.clone())
        .await;
    sess.send_event(
        turn_context,
        EventMsg::Error(ErrorEvent {
            message: message.into(),
            codex_error_info: Some(error),
        }),
    )
    .await;
}

async fn report_logical_generation_budget_exhausted(
    sess: &Session,
    turn_context: &TurnContext,
    reported: &mut bool,
) {
    if std::mem::replace(reported, true) {
        return;
    }
    emit_status_affecting_turn_error(
        sess,
        turn_context,
        LOGICAL_GENERATION_BUDGET_EXHAUSTED_MESSAGE,
    )
    .await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegularFollowUpAdmission {
    Admit,
    Exhausted,
}

fn regular_follow_up_admission(
    budget: &LogicalGenerationBudget,
    exhaustion_reported: bool,
) -> RegularFollowUpAdmission {
    if exhaustion_reported || !budget.can_admit(/*terminal_requested*/ false) {
        RegularFollowUpAdmission::Exhausted
    } else {
        RegularFollowUpAdmission::Admit
    }
}

async fn admit_regular_follow_up(
    sess: &Session,
    turn_context: &TurnContext,
    budget: &LogicalGenerationBudget,
    exhaustion_reported: &mut bool,
) -> bool {
    match regular_follow_up_admission(budget, *exhaustion_reported) {
        RegularFollowUpAdmission::Admit => true,
        RegularFollowUpAdmission::Exhausted => {
            report_logical_generation_budget_exhausted(sess, turn_context, exhaustion_reported)
                .await;
            false
        }
    }
}

async fn drain_pending_input_if_generation_available<F>(
    budget: &LogicalGenerationBudget,
    pending_input: F,
) -> Option<Vec<TurnInput>>
where
    F: Future<Output = Vec<TurnInput>>,
{
    if budget.is_exhausted() {
        None
    } else if !budget.has_regular_generation_capacity() {
        Some(Vec::new())
    } else {
        Some(pending_input.await)
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

fn protocol_resample_completion_allowed(server_resample_eligible: bool) -> bool {
    server_resample_eligible
}

fn generation_request_action_changed(
    generation_request: &GenerationRequestDisposition,
    next_generation_request: Option<&GenerationRequestDisposition>,
) -> bool {
    next_generation_request.is_none_or(|next| next != generation_request)
}

fn take_convergence_observation(
    decision: Option<&mut SamplingConvergenceDecision>,
) -> (bool, Option<String>) {
    decision
        .map(|decision| {
            (
                std::mem::take(&mut decision.proven_loop_activated),
                decision.directive.take(),
            )
        })
        .unwrap_or_default()
}

async fn record_convergence_decision(
    sess: &Session,
    turn_context: &TurnContext,
    decision: Option<&mut SamplingConvergenceDecision>,
) {
    let (proven_loop_activated, directive) = take_convergence_observation(decision);
    if proven_loop_activated {
        turn_context
            .turn_timing_state
            .record_proven_loop_activation();
    }
    let Some(directive) = directive else {
        return;
    };
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
    sess.record_conversation_items(turn_context, std::slice::from_ref(&directive_item))
        .await;
}

struct CompletionStopHookReport {
    continuation_prompt: Option<ResponseItem>,
    should_stop: bool,
    stop_reason: Option<String>,
}

async fn run_completion_stop_hook(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    stop_hook_active: bool,
    last_agent_message: Option<String>,
) -> CompletionStopHookReport {
    let observed =
        run_turn_stop_hooks(sess, turn_context, stop_hook_active, last_agent_message).await;
    let stop = observed.stop;
    let continuation_prompt = stop
        .should_block
        .then(|| build_hook_prompt_message(&stop.continuation_fragments))
        .flatten();
    if stop.should_block && continuation_prompt.is_none() {
        let reason = stop
            .block_reason
            .as_deref()
            .filter(|reason| !reason.trim().is_empty())
            .map(|reason| format!(": {reason}"))
            .unwrap_or_default();
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!(
                    "Stop hook requested continuation without a prompt{reason}; ignoring the block."
                ),
            }),
        )
        .await;
    }
    CompletionStopHookReport {
        continuation_prompt,
        should_stop: stop.should_stop,
        stop_reason: stop.stop_reason,
    }
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
    accepted_context_input: bool,
}

fn resets_reasoning_governor(input: &TurnInput) -> bool {
    match input {
        TurnInput::UserInput { content, .. } => !content.is_empty(),
        TurnInput::ResponseItem(_) | TurnInput::InterAgentCommunication(_) => true,
    }
}

async fn run_hooks_and_record_inputs_detailed(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
) -> RecordedInputOutcome {
    let mut blocked_input = false;
    let mut accepted_context_input = false;
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
                accepted_context_input = true;
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
        should_stop: blocked_input && !accepted_context_input,
        accepted_context_input,
    }
}

struct PendingTurnPlan {
    planning_generation: u64,
    step_context: Arc<StepContext>,
    prepared_context_update: Option<PreparedContextUpdate>,
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

fn take_stabilized_context_update(
    prepared_context_update: &mut Option<PreparedContextUpdate>,
) -> CodexResult<PreparedContextUpdate> {
    prepared_context_update.take().ok_or_else(|| {
        CodexErr::Fatal(
            "a stabilized pending-turn plan did not carry its context candidate".to_string(),
        )
    })
}

enum PendingTurnPlanBuild {
    Stale,
    Ready(Box<PendingTurnPlan>),
}

#[derive(Default)]
struct SamplingAttemptProgress {
    accepted_output: bool,
}

impl SamplingAttemptProgress {
    fn requires_authoritative_retry_input(&self) -> bool {
        self.accepted_output
    }
}

struct AdvertisedDeferredToolLease {
    turn_context: Arc<TurnContext>,
    advertised: HashSet<ToolName>,
}

impl AdvertisedDeferredToolLease {
    fn new(turn_context: Arc<TurnContext>, advertised: HashSet<ToolName>) -> Self {
        Self {
            turn_context,
            advertised,
        }
    }
}

impl Drop for AdvertisedDeferredToolLease {
    fn drop(&mut self) {
        self.turn_context
            .release_advertised_deferred_tools(&self.advertised);
    }
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
        let (first_router, prepared_context_update) = tokio::join!(
            built_tools_for_pending_turn(
                sess.as_ref(),
                step_context.as_ref(),
                &[],
                planning_generation,
                cancellation_token
            ),
            sess.prepare_context_update(step_context.as_ref()),
        );
        let first_router = first_router?;
        if sess.services.planning_generation() != planning_generation {
            return Ok(PendingTurnPlanBuild::Stale);
        }
        let initial_context = !sess.has_reference_context_item().await;
        let pending_token_estimate = estimate_pending_tokens(
            input,
            &[],
            prepared_context_update.context_items(),
            first_router.as_ref(),
            initial_context,
        );
        let projected_prompt_pressure =
            projected_prompt_pressure(sess, turn_context, pending_token_estimate).await;
        let warnings = first_router.planning_warnings().to_vec();
        return Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
            planning_generation,
            step_context,
            prepared_context_update: Some(prepared_context_update),
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
    let connector_snapshot = step_context.mcp.config().connector_snapshot.clone();
    let (recommended_plugin_items, mcp_tools) = join_recommendations_and_mcp(
        build_recommended_plugin_items(
            sess,
            turn_context,
            &loaded_plugins,
            &recommended_plugin_input,
        ),
        async {
            if turn_context.apps_enabled() || !mentioned_plugins.is_empty() {
                step_context.mcp_tools().or_cancel(cancellation_token).await
            } else {
                Ok(&[][..])
            }
        },
    )
    .await;
    let mcp_tools = mcp_tools?;
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
        .map(|skill| (skill.role(), skill.render()))
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
                .map(|skill| (skill.role(), skill.render())),
        ),
        None => skill_items,
    };
    let base_instructions = Arc::clone(&turn_context.base_instructions);
    if !base_instructions_own_task_model_guidance(&base_instructions.text) {
        injection_items.insert(0, ContextualUserFragment::into(TaskModelGuidance));
    }
    injection_items.extend(recommended_plugin_items);
    injection_items.extend(plugin_items);
    injection_items.extend(extension_injection_items);
    for item in &mut injection_items {
        mark_trusted_stable_context_item(item);
    }
    if !injection_items.is_empty() {
        let history = sess.clone_history().await;
        injection_items =
            filter_unchanged_stable_context_items(history.raw_items(), injection_items);
    }

    // Final DAG leaves build the router and immutable context candidate concurrently,
    // then validate the generation before accepting either.
    let (first_router, prepared_context_update) = tokio::join!(
        built_tools_for_pending_turn(
            sess.as_ref(),
            step_context.as_ref(),
            &skill_plan.invocations,
            planning_generation,
            cancellation_token,
        ),
        sess.prepare_context_update(step_context.as_ref()),
    );
    let first_router = first_router?;
    if sess.services.planning_generation() != planning_generation {
        return Ok(PendingTurnPlanBuild::Stale);
    }
    let mut warnings = planned_mcp.warnings;
    warnings.extend(first_router.planning_warnings().iter().cloned());
    warnings.extend(skill_plan.injections.warnings.iter().cloned());
    let initial_context = !sess.has_reference_context_item().await;
    let pending_token_estimate = estimate_pending_tokens(
        input,
        &injection_items,
        prepared_context_update.context_items(),
        first_router.as_ref(),
        initial_context,
    );
    let projected_prompt_pressure =
        projected_prompt_pressure(sess, turn_context, pending_token_estimate).await;
    Ok(PendingTurnPlanBuild::Ready(Box::new(PendingTurnPlan {
        planning_generation,
        step_context,
        prepared_context_update: Some(prepared_context_update),
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
    planning_iterations: &mut usize,
    completed_mcp_effect: &mut Option<(String, Option<HashSet<String>>)>,
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
            cancellation_token,
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

        if let Some((effect_id, Some(expected_inventory_keys))) = completed_mcp_effect
            && !inventory_contains_expected(sess, expected_inventory_keys).await
        {
            return Err(planning_failure(format!(
                "completed inventory effect `{effect_id}` is missing its expected model-visible state"
            )));
        }

        let planning_generation = sess.services.planning_generation();
        let step_context = sess.capture_step_context(Arc::clone(turn_context)).await;
        let plan_build = charge_pending_turn_plan_build(
            build_pure_pending_turn_plan(
                sess,
                step_context,
                input,
                planning_generation,
                cancellation_token,
            )
            .await?,
            planning_iterations,
        )
        .map_err(|message| planning_failure_with_timing(turn_context, message))?;
        let plan = match plan_build {
            PendingTurnPlanBuild::Stale => continue,
            PendingTurnPlanBuild::Ready(plan) => *plan,
        };
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
            Arc::clone(&plan.step_context),
            plan.projected_prompt_pressure,
            !incoming_precompaction_completed,
            cancellation_token,
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
            && !mcp_dependency_effect_is_completed(completed_mcp_effect.as_ref(), &effect.id)
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
            let expected_inventory_keys = match outcome {
                McpDependencyEffectOutcome::Skipped => None,
                McpDependencyEffectOutcome::InventoryChanged {
                    expected_inventory_keys,
                } => Some(expected_inventory_keys),
            };
            let inventory_changed = expected_inventory_keys.is_some();
            *completed_mcp_effect = Some((effect.id.clone(), expected_inventory_keys));
            turn_context
                .turn_timing_state
                .record_planning_semantic_effect();
            if inventory_changed {
                client_session.invalidate_incremental_history("model-visible planning effect");
                turn_context
                    .turn_timing_state
                    .record_planning_invalidation();
                require_newer_planning_generation(
                    plan.planning_generation,
                    sess.services.planning_generation(),
                )
                .map_err(|message| planning_failure_with_timing(turn_context, message))?;
                continue;
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

const MAX_PENDING_TURN_PLAN_ITERATIONS: usize = 8;

fn advance_pending_turn_plan_iteration(iterations: &mut usize) -> Result<(), String> {
    *iterations = iterations.saturating_add(1);
    if *iterations > MAX_PENDING_TURN_PLAN_ITERATIONS {
        return Err(format!(
            "pending-turn planning did not stabilize after {MAX_PENDING_TURN_PLAN_ITERATIONS} iterations"
        ));
    }
    Ok(())
}

fn charge_pending_turn_plan_build<T>(build: T, iterations: &mut usize) -> Result<T, String> {
    advance_pending_turn_plan_iteration(iterations)?;
    Ok(build)
}

fn mcp_dependency_effect_is_completed(
    completed_effect: Option<&(String, Option<HashSet<String>>)>,
    effect_id: &str,
) -> bool {
    completed_effect.is_some_and(|(completed_id, _)| completed_id == effect_id)
}

fn require_newer_planning_generation(
    before_generation: u64,
    after_generation: u64,
) -> Result<(), String> {
    if after_generation <= before_generation {
        return Err(format!(
            "inventory effect completed at planning generation {before_generation}, but the next observable generation did not advance"
        ));
    }
    Ok(())
}

async fn commit_pending_turn_plan_effects(
    sess: &Session,
    turn_context: &TurnContext,
    plan: &PendingTurnPlan,
) {
    for message in &plan.warnings {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: message.clone(),
            }),
        )
        .await;
        turn_context
            .turn_timing_state
            .record_planning_semantic_effect();
    }

    if !plan.skill_plan.invocations.is_empty() || !plan.skill_plan.metrics.is_empty() {
        apply_skill_injection_observability(
            &plan.skill_plan,
            Some(&turn_context.session_telemetry),
            &sess.services.analytics_events_client,
            plan.tracking.clone(),
        );
        turn_context
            .turn_timing_state
            .record_planning_semantic_effect();
    }

    if !plan.mentioned_apps.is_empty() {
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
        turn_context
            .turn_timing_state
            .record_planning_semantic_effect();
    }

    if !plan.mentioned_plugins.is_empty() {
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
        turn_context
            .turn_timing_state
            .record_planning_semantic_effect();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTokenEstimate {
    total_tokens: i64,
    body_growth_tokens: i64,
    resolves_active_reasoning: bool,
}

#[derive(Default)]
struct SerializedByteCounter {
    bytes: usize,
}

impl std::io::Write for SerializedByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_json_len<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<usize> {
    let mut counter = SerializedByteCounter::default();
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}

fn injection_serialized_lengths(
    injection_items: &[ResponseItem],
) -> serde_json::Result<(usize, usize)> {
    let mut item_bytes = 0usize;
    let mut body_item_bytes = 0usize;
    for item in injection_items {
        let serialized_len = serialized_json_len(item)?;
        item_bytes = item_bytes.saturating_add(serialized_len);
        let is_stable_startup_item = matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if role == "user"
                    && !content.is_empty()
                    && content.iter().all(is_startup_contextual_user_fragment)
        );
        if !is_stable_startup_item {
            body_item_bytes = body_item_bytes.saturating_add(serialized_len);
        }
    }
    let array_delimiters_and_commas =
        2usize.saturating_add(injection_items.len().saturating_sub(1));
    Ok((
        item_bytes.saturating_add(array_delimiters_and_commas),
        body_item_bytes,
    ))
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
            TurnInput::UserInput { content, .. } => serialized_json_len(content),
            TurnInput::ResponseItem(item) => serialized_json_len(item),
            TurnInput::InterAgentCommunication(communication) => {
                serialized_json_len(&communication.to_model_input_item())
            }
        }
        .unwrap_or_default();
        bytes.saturating_add(item_bytes)
    });
    let (injection_bytes, body_injection_bytes) =
        injection_serialized_lengths(injection_items).unwrap_or_default();
    let context_update_bytes = serialized_json_len(context_update_items).unwrap_or_default();
    let tool_bytes = router.model_visible_schemas().serialized().len();
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
        .saturating_add(body_injection_bytes)
        .saturating_add(context_growth_bytes);
    PendingTokenEstimate {
        total_tokens: i64::try_from(total_bytes.div_ceil(4)).unwrap_or(i64::MAX),
        body_growth_tokens: i64::try_from(body_growth_bytes.div_ceil(4)).unwrap_or(i64::MAX),
        resolves_active_reasoning: input.iter().any(|item| match item {
            TurnInput::UserInput { .. } | TurnInput::InterAgentCommunication(_) => true,
            TurnInput::ResponseItem(item) => crate::context_manager::is_user_turn_boundary(item),
        }),
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
    let body_after_prefix = matches!(
        turn_context.config.model_auto_compact_token_limit_scope,
        AutoCompactTokenLimitScope::BodyAfterPrefix
    );
    let (active_context_tokens, committed_history_tokens, auto_compact_window) =
        collect_projected_prompt_state(
            sess.get_total_token_usage(),
            async {
                if pending_token_estimate.resolves_active_reasoning {
                    sess.get_estimated_token_count_after_pending_user_boundary(turn_context)
                        .await
                } else {
                    sess.get_estimated_token_count(turn_context).await
                }
            },
            async {
                if body_after_prefix {
                    Some(sess.auto_compact_window_snapshot().await)
                } else {
                    None
                }
            },
        )
        .await;
    let committed_history_tokens = committed_history_tokens.unwrap_or(active_context_tokens);
    let total_tokens = projected_prompt_tokens_from_estimates(
        active_context_tokens,
        committed_history_tokens,
        pending_token_estimate.total_tokens,
        pending_token_estimate.body_growth_tokens,
    );
    let auto_compact_scope_tokens = match turn_context.config.model_auto_compact_token_limit_scope {
        AutoCompactTokenLimitScope::Total => total_tokens,
        AutoCompactTokenLimitScope::BodyAfterPrefix => {
            let baseline = auto_compact_window
                .and_then(|window| window.prefill_input_tokens)
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
    pending_body_growth_tokens: i64,
) -> i64 {
    let locally_estimated_prompt = committed_history_tokens.saturating_add(pending_token_estimate);
    let server_usage_with_pending_body =
        active_context_tokens.saturating_add(pending_body_growth_tokens);
    locally_estimated_prompt.max(server_usage_with_pending_body)
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
            submission_type: Some(turn_submission_type(input)),
            ephemeral: thread_config.ephemeral,
            session_source: thread_config.session_source,
            model: turn_context.model_info.slug.clone(),
            model_provider: turn_context.config.model_provider_id.clone(),
            permission_profile: turn_context.permission_profile(),
            permission_profile_cwd: turn_context.cwd().to_path_buf(),
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

fn turn_submission_type(input: &[TurnInput]) -> TurnSubmissionType {
    if input.is_empty() {
        TurnSubmissionType::Queued
    } else {
        TurnSubmissionType::Default
    }
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
    cancellation_token: &CancellationToken,
) -> CodexResult<HistoryPreSamplingCompaction> {
    if check_previous_model
        && maybe_run_previous_model_inline_compact(
            sess,
            turn_context,
            client_session,
            cancellation_token,
        )
        .await?
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
    step_context: Arc<StepContext>,
    projected_prompt_pressure: ProjectedPromptPressure,
    allow_pending_input_compaction: bool,
    cancellation_token: &CancellationToken,
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

    // Reuse the step context the pending-turn plan was built from. Capturing another one
    // would re-run AGENTS.md discovery and MCP runtime resolution for the same step, and any
    // compaction invalidates this plan and restarts the loop with a fresh capture anyway.
    run_auto_compact(
        sess,
        step_context,
        /*fallback_step_context*/ None,
        client_session,
        /*prefetched_workspace_identity*/ None,
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::PreTurn,
        cancellation_token,
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
    cancellation_token: &CancellationToken,
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
            /*prefetched_workspace_identity*/ None,
            InitialContextInjection::DoNotInject,
            CompactionReason::CompHashChanged,
            CompactionPhase::PreTurn,
            cancellation_token,
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
            /*prefetched_workspace_identity*/ None,
            InitialContextInjection::DoNotInject,
            CompactionReason::ModelDownshift,
            CompactionPhase::PreTurn,
            cancellation_token,
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
#[allow(clippy::too_many_arguments)]
async fn run_auto_compact(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    client_session: &mut ModelClientSession,
    prefetched_workspace_identity: Option<&Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    initial_context_injection: InitialContextInjection,
    reason: CompactionReason,
    phase: CompactionPhase,
    cancellation_token: &CancellationToken,
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
    if should_use_remote_compact_task(
        turn_context.provider.info(),
        turn_context.config.compact_prompt.as_deref(),
    ) {
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
            cancellation_token,
        )
        .await?;
    } else {
        emit_compact_metric(
            &sess.services.session_telemetry,
            "local",
            /*manual*/ false,
        );
        let previous_model = turn_context.model_info.slug.as_str();
        let initial_attempt = run_inline_auto_compact_task(
            Arc::clone(sess),
            Arc::clone(turn_context),
            InlineAutoCompactReuse {
                // Reuse the turn transport, matching the remote-compaction path above.
                client_session,
                prefetched_workspace_identity,
            },
            initial_context_injection.clone(),
            reason,
            phase,
            /*emit_error_event*/ fallback_step_context.is_none(),
            cancellation_token,
        )
        .await;

        match initial_attempt {
            Ok(()) => {}
            Err(previous_error) => {
                let Some(fallback_step_context) = fallback_step_context else {
                    return Err(previous_error);
                };
                if !should_retry_with_current_model(&previous_error) {
                    return Err(previous_error);
                }

                let fallback_turn_context = &fallback_step_context.turn;
                let fallback_result = run_inline_auto_compact_task(
                    Arc::clone(sess),
                    Arc::clone(fallback_turn_context),
                    InlineAutoCompactReuse {
                        client_session,
                        prefetched_workspace_identity,
                    },
                    initial_context_injection,
                    reason,
                    phase,
                    /*emit_error_event*/ true,
                    cancellation_token,
                )
                .await;
                record_model_fallback(
                    &sess.services.session_telemetry,
                    previous_model,
                    fallback_turn_context.model_info.slug.as_str(),
                    reason,
                    CompactionImplementation::Responses,
                    fallback_result.as_ref().err(),
                );
                if let Err(fallback_error) = &fallback_result {
                    sess.send_event(
                        fallback_turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Compaction failed with the previous model: {previous_error}; retry with the current model also failed: {fallback_error}"
                            ),
                        }),
                    )
                    .await;
                }
                fallback_result?;
            }
        }
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

#[derive(Debug)]
struct CompactedProjectedPromptInputs {
    input: Arc<[ResponseItem]>,
    stable_context_fallback_input: Arc<[ResponseItem]>,
    tool_history_fallback_input: Arc<[ResponseItem]>,
    stable_context_tool_history_fallback_input: Arc<[ResponseItem]>,
    #[cfg(test)]
    pass_count: usize,
}

fn compact_projected_prompt_inputs(
    prepared: &PreparedPromptInput,
) -> CompactedProjectedPromptInputs {
    let [
        input,
        stable_context_fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
    ] = prepared.compacted_tool_search_outputs(compact_acknowledged_tool_search_outputs);
    #[cfg(test)]
    let pass_count = {
        let compacted = [
            &input,
            &stable_context_fallback_input,
            &tool_history_fallback_input,
            &stable_context_tool_history_fallback_input,
        ];
        compacted
            .iter()
            .enumerate()
            .filter(|(index, candidate)| {
                !compacted[..*index]
                    .iter()
                    .any(|prior| Arc::ptr_eq(prior, candidate))
            })
            .count()
    };

    CompactedProjectedPromptInputs {
        input,
        stable_context_fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
        #[cfg(test)]
        pass_count,
    }
}

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_prompt(
    input: impl Into<Arc<[ResponseItem]>>,
    router: &ToolRouter,
    turn_context: &TurnContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    let input = compact_acknowledged_tool_search_outputs(input.into());
    Prompt {
        input: Arc::clone(&input),
        stable_context_fallback_input: Arc::clone(&input),
        tool_history_fallback_input: Arc::clone(&input),
        stable_context_tool_history_fallback_input: input,
        tool_history_substitutions: Arc::from([]),
        stable_context_fallback_tool_history_substitutions: Arc::from([]),
        stable_context_manifest: Default::default(),
        deferred_dynamic_history: None,
        prompt_provenance: Default::default(),
        digests: PromptDigests::default(),
        tools: router.model_visible_schemas_for_turn(turn_context),
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        base_instructions,
        output_schema: turn_context.final_output_json_schema.clone(),
        output_schema_strict: !crate::guardian::is_guardian_reviewer_source(
            &turn_context.session_source,
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RepositoryScaffoldIdentity {
    identity: [u8; 32],
    semantic_replacement: bool,
}

#[derive(Debug)]
struct RequestScaffoldOwner {
    config: Arc<crate::config::Config>,
    exec_policy: Option<Arc<codex_execpolicy::Policy>>,
    model_slug: String,
    stable_context_manifest: crate::stable_context::StableContextManifest,
    repository: Option<RepositoryScaffoldIdentity>,
}

impl RequestScaffoldOwner {
    fn matches(
        &self,
        prepared: &PreparedPromptInput,
        step_context: &StepContext,
        exec_policy: Option<&Arc<codex_execpolicy::Policy>>,
    ) -> bool {
        (Arc::ptr_eq(&self.config, &step_context.turn.config)
            || self.config.as_ref() == step_context.turn.config.as_ref())
            && match (&self.exec_policy, exec_policy) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && self.model_slug == step_context.turn.model_info.slug
            && stable_context_owner_matches(
                &self.stable_context_manifest,
                prepared.stable_context_manifest(),
            )
            && self.repository == repository_scaffold_identity(step_context)
    }
}

#[derive(Debug)]
struct RequestScaffold {
    base_instructions: BaseInstructions,
    tools: Arc<ToolSchemaArtifact>,
    stable_context_manifest: crate::stable_context::StableContextManifest,
    reused_stable_context_manifest: crate::stable_context::StableContextManifest,
    stable_input_bytes: u64,
    stable_input_tokens: i64,
    digests: PromptDigests,
    exact_provenance_fragments: Vec<(String, PromptContextCategory)>,
}

impl RequestScaffold {
    fn build(
        prepared: &PreparedPromptInput,
        step_context: &StepContext,
        tools: Arc<ToolSchemaArtifact>,
        base_instructions: BaseInstructions,
        exec_policy: Option<&codex_execpolicy::Policy>,
    ) -> Self {
        let mut manifest = prepared.stable_context_manifest().with_repository_identity(
            step_context
                .agents_md_stable_context
                .as_ref()
                .map(RepositoryStableContextBundle::metadata),
        );
        let stable_input_bytes = manifest.projected_bytes();
        let stable_input_tokens = manifest.projected_tokens();
        manifest = manifest
            .with_base_model(&step_context.turn.model_info.slug, &base_instructions.text)
            .add_component_bytes(
                StableContextKind::ToolSchemas,
                "tool_schemas",
                tools.serialized(),
            );
        if tools.has_request_user_input() {
            manifest = manifest.add_measured_component(
                StableContextKind::RequestUserInput,
                "request_user_input_available",
                b"request_user_input",
                0,
                0,
            );
        }
        if tools.has_wait() {
            manifest = manifest.add_measured_component(
                StableContextKind::Wait,
                "wait_available",
                b"wait",
                0,
                0,
            );
        }
        let digests = PromptDigests {
            instructions: manifest.active_content_hash(StableContextKind::BaseModel),
            tools: Some(tools.digest()),
            history: None,
        };
        let reused_stable_context_manifest = manifest.with_local_reused(true);
        let mut exact_provenance_fragments = Vec::with_capacity(3);
        if step_context.turn.config.include_permissions_instructions {
            let Some(exec_policy) = exec_policy else {
                unreachable!("permissions scaffold requires the current exec policy");
            };
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
                exec_policy,
                step_context.turn.cwd(),
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
            exact_provenance_fragments
                .push((permissions, PromptContextCategory::EnvironmentPermissions));
        }
        if let Some(role_policy) = crate::session::multi_agents::usage_hint_text(
            &step_context.turn,
            &step_context.turn.session_source,
        ) {
            exact_provenance_fragments
                .push((role_policy.to_string(), PromptContextCategory::AgentRole));
        }
        if matches!(step_context.turn.session_source, SessionSource::VSCode)
            && let Some(desktop_context) = step_context.turn.developer_instructions.as_deref()
        {
            exact_provenance_fragments.push((
                desktop_context.to_string(),
                PromptContextCategory::AppDesktop,
            ));
        }
        Self {
            base_instructions,
            tools,
            stable_context_manifest: manifest,
            reused_stable_context_manifest,
            stable_input_bytes,
            stable_input_tokens,
            digests,
            exact_provenance_fragments,
        }
    }

    fn manifest(&self, locally_reused: bool) -> &crate::stable_context::StableContextManifest {
        if locally_reused {
            &self.reused_stable_context_manifest
        } else {
            &self.stable_context_manifest
        }
    }
}

#[derive(Debug)]
struct ResolvedRequestScaffold {
    scaffold: Arc<RequestScaffold>,
    locally_reused: bool,
}

/// Cheap identity for the model-visible tool surface of one request.
///
/// Materializing the schema artifact walks the registry and takes the router schema lock, so
/// the scaffold cache compares this token first and only acquires the artifact on a miss.
/// `router` is the registry identity and `activation_revision` advances whenever deferred
/// tool activation changes the exposed surface, which together determine the artifact the
/// router would hand back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolSchemaSurfaceToken {
    router: *const ToolRouter,
    activation_revision: u64,
    terminal_completion_only: bool,
}

// The pointer is only ever compared for identity; it is never dereferenced.
unsafe impl Send for ToolSchemaSurfaceToken {}
unsafe impl Sync for ToolSchemaSurfaceToken {}

impl ToolSchemaSurfaceToken {
    fn capture(
        router: &ToolRouter,
        turn: &crate::session::turn_context::TurnContext,
        terminal_completion_only: bool,
    ) -> Self {
        Self {
            router: std::ptr::from_ref(router),
            activation_revision: turn.deferred_tool_activation_revision(),
            terminal_completion_only,
        }
    }
}

#[derive(Debug)]
struct RequestScaffoldCacheEntry {
    owner: RequestScaffoldOwner,
    scaffold: Arc<RequestScaffold>,
    tool_schema_surface: ToolSchemaSurfaceToken,
}

#[derive(Debug, Default)]
pub(super) struct RequestScaffoldCache {
    entry: Option<RequestScaffoldCacheEntry>,
    #[cfg(test)]
    build_count: usize,
}

impl RequestScaffoldCache {
    fn resolve(
        &mut self,
        prepared: &PreparedPromptInput,
        sess: &Session,
        router: &ToolRouter,
        step_context: &StepContext,
        base_instructions: &BaseInstructions,
        terminal_completion_only: bool,
    ) -> ResolvedRequestScaffold {
        let exec_policy = step_context
            .turn
            .config
            .include_permissions_instructions
            .then(|| sess.services.exec_policy.current());
        let tool_schema_surface =
            ToolSchemaSurfaceToken::capture(router, &step_context.turn, terminal_completion_only);
        // Check the cheap surface token before materializing the schema artifact so a repeated
        // request with an unchanged tool surface never touches the router schema cache.
        if let Some(entry) = self.entry.as_ref()
            && entry.tool_schema_surface == tool_schema_surface
            && entry
                .owner
                .matches(prepared, step_context, exec_policy.as_ref())
            && entry.scaffold.base_instructions == *base_instructions
        {
            return ResolvedRequestScaffold {
                scaffold: Arc::clone(&entry.scaffold),
                locally_reused: true,
            };
        }

        let tools = if terminal_completion_only {
            Arc::new(ToolSchemaArtifact::default())
        } else {
            router.model_visible_schemas_for_turn(&step_context.turn)
        };
        // A different router instance or activation revision can still project the same
        // surface. Fall back to the artifact comparison so those requests keep reusing the
        // scaffold instead of rebuilding it.
        if let Some(entry) = self.entry.as_mut()
            && entry
                .owner
                .matches(prepared, step_context, exec_policy.as_ref())
            && entry.scaffold.base_instructions == *base_instructions
            && tool_schema_surface_matches(&entry.scaffold.tools, &tools)
        {
            entry.tool_schema_surface = tool_schema_surface;
            return ResolvedRequestScaffold {
                scaffold: Arc::clone(&entry.scaffold),
                locally_reused: true,
            };
        }

        let owner = RequestScaffoldOwner {
            config: Arc::clone(&step_context.turn.config),
            exec_policy: exec_policy.as_ref().map(Arc::clone),
            model_slug: step_context.turn.model_info.slug.clone(),
            stable_context_manifest: prepared.stable_context_manifest().clone(),
            repository: repository_scaffold_identity(step_context),
        };
        let scaffold = Arc::new(RequestScaffold::build(
            prepared,
            step_context,
            tools,
            base_instructions.clone(),
            exec_policy.as_deref(),
        ));
        self.entry = Some(RequestScaffoldCacheEntry {
            owner,
            scaffold: Arc::clone(&scaffold),
            tool_schema_surface,
        });
        #[cfg(test)]
        {
            self.build_count = self.build_count.saturating_add(1);
        }
        ResolvedRequestScaffold {
            scaffold,
            locally_reused: false,
        }
    }

    #[cfg(test)]
    fn build_count(&self) -> usize {
        self.build_count
    }
}

fn arc_identity_or_equivalent<T: ?Sized>(
    left: &Arc<T>,
    right: &Arc<T>,
    equivalent: impl FnOnce(&T, &T) -> bool,
) -> bool {
    Arc::ptr_eq(left, right) || equivalent(left.as_ref(), right.as_ref())
}

fn tool_schema_surface_matches(
    left: &Arc<ToolSchemaArtifact>,
    right: &Arc<ToolSchemaArtifact>,
) -> bool {
    arc_identity_or_equivalent(left, right, |left, right| {
        left.digest() == right.digest() && left.serialized() == right.serialized()
    })
}

fn repository_scaffold_identity(step_context: &StepContext) -> Option<RepositoryScaffoldIdentity> {
    step_context
        .agents_md_stable_context
        .as_ref()
        .map(|bundle| {
            let (identity, _locally_reused, semantic_replacement) = bundle.metadata();
            RepositoryScaffoldIdentity {
                identity,
                semantic_replacement,
            }
        })
}

fn stable_context_owner_matches(
    left: &crate::stable_context::StableContextManifest,
    right: &crate::stable_context::StableContextManifest,
) -> bool {
    left.projection_enabled() == right.projection_enabled()
        && left.fail_open() == right.fail_open()
        && left.components().len() == right.components().len()
        && left
            .components()
            .iter()
            .zip(right.components())
            .all(|(left, right)| {
                left.kind == right.kind
                    && left.identity == right.identity
                    && left.active == right.active
                    && left.disposition == right.disposition
            })
}

pub(crate) fn build_projected_prompt(
    sess: &Session,
    prepared: &PreparedPromptInput,
    router: &ToolRouter,
    step_context: &StepContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    let scaffold = ResolvedRequestScaffold {
        scaffold: Arc::new(RequestScaffold::build(
            prepared,
            step_context,
            router.model_visible_schemas_for_turn(&step_context.turn),
            base_instructions,
            step_context
                .turn
                .config
                .include_permissions_instructions
                .then(|| sess.services.exec_policy.current())
                .as_deref(),
        )),
        locally_reused: false,
    };
    build_projected_prompt_from_scaffold(sess, prepared, step_context, &scaffold)
}

fn build_projected_prompt_from_scaffold(
    _sess: &Session,
    prepared: &PreparedPromptInput,
    step_context: &StepContext,
    resolved_scaffold: &ResolvedRequestScaffold,
) -> Prompt {
    let scaffold = resolved_scaffold.scaffold.as_ref();
    let CompactedProjectedPromptInputs {
        input,
        stable_context_fallback_input: fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
        #[cfg(test)]
            pass_count: _,
    } = compact_projected_prompt_inputs(prepared);
    let digests = PromptDigests {
        history: prepared.fingerprint(),
        ..scaffold.digests
    };
    let prompt_provenance = prepared.prompt_provenance().with_exact_fragments(
        &input,
        scaffold
            .exact_provenance_fragments
            .iter()
            .map(|(fragment, category)| (fragment.as_str(), *category)),
    );
    Prompt {
        input,
        stable_context_fallback_input: fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
        tool_history_substitutions: prepared.tool_history_substitutions(),
        stable_context_fallback_tool_history_substitutions: prepared
            .fallback_tool_history_substitutions(),
        stable_context_manifest: scaffold.manifest(resolved_scaffold.locally_reused).clone(),
        deferred_dynamic_history: Some(
            crate::client_common::DeferredDynamicHistoryMeasurement::new(
                scaffold.stable_input_bytes,
                scaffold.stable_input_tokens,
            ),
        ),
        prompt_provenance,
        digests,
        tools: Arc::clone(&scaffold.tools),
        parallel_tool_calls: step_context.turn.model_info.supports_parallel_tool_calls,
        base_instructions: scaffold.base_instructions.clone(),
        output_schema: step_context.turn.final_output_json_schema.clone(),
        output_schema_strict: !crate::guardian::is_guardian_reviewer_source(
            &step_context.turn.session_source,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %step_context.turn.sub_id,
        model = %step_context.turn.model_info.slug,
        cwd = %step_context.turn.cwd().display()
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
    base_instructions: &BaseInstructions,
    preparation_timing_guard: &mut Option<TurnTimingGuard>,
    reasoning_phase: Option<SamplingReasoningPhase>,
    reasoning_trigger: codex_protocol::protocol::ReasoningPolicyTrigger,
    generation_request: GenerationRequestDisposition,
    generation_id: ModelGenerationId,
    request_signals: SamplingRequestSignalCollector,
    pending_continuation_cause: &mut Option<ContinuationCause>,
    cancellation_token: CancellationToken,
) -> CodexResult<(SamplingRequestResult, Arc<[ResponseItem]>)> {
    let turn_context = Arc::clone(&step_context.turn);
    let terminal_completion_only = generation_request.terminal_completion_only;
    // Record the deferred schemas advertised by this request. Settling the guard preserves them
    // for later continuations in the same turn; capability refresh and turn teardown own expiry.
    let _advertised_deferred_tool_lease = (!terminal_completion_only).then(|| {
        let advertised_deferred_tools = turn_context.activated_deferred_tools();
        AdvertisedDeferredToolLease::new(Arc::clone(&turn_context), advertised_deferred_tools)
    });
    let cached_router = prebuilt_router.take();
    let router = match cached_router {
        Some(router) if terminal_completion_only => {
            // A terminal continuation advertises no tools, so exposure drift
            // cannot affect this request. Reuse the already-finalized router
            // only as a defensive sink for an invalid model-emitted tool call.
            turn_context.turn_timing_state.record_tool_router_reuse();
            router
        }
        Some(router)
            if finalized_router_matches_current_exposure(
                sess.as_ref(),
                step_context.as_ref(),
                router.as_ref(),
            )
            .await =>
        {
            turn_context.turn_timing_state.record_tool_router_reuse();
            router
        }
        Some(_) => {
            turn_context.turn_timing_state.record_tool_router_rebuild();
            built_tools(
                sess.as_ref(),
                step_context.as_ref(),
                selected_skill_invocations,
                &cancellation_token,
            )
            .await?
        }
        None => {
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
    let request_scaffold = sess
        .request_scaffold_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resolve(
            &prepared_input,
            sess.as_ref(),
            router.as_ref(),
            step_context.as_ref(),
            base_instructions,
            terminal_completion_only,
        );
    let pending_tool_manifest = Arc::new(Mutex::new(if !terminal_completion_only {
        let previous_manifest_hash = sess
            .queued_tool_manifest_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let manifest = router
            .tool_manifest_for_rollout(turn_context.as_ref(), previous_manifest_hash.as_deref());
        Some(manifest)
    } else {
        None
    }));

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&sess),
        Arc::clone(&step_context),
        Arc::clone(&turn_diff_tracker),
    )
    .with_sampling_request_signals(request_signals.clone());
    let _code_mode_worker = (!terminal_completion_only).then(|| {
        sess.services.code_mode_service.start_turn_worker(
            &sess,
            Arc::clone(&step_context),
            Arc::clone(&turn_diff_tracker),
            request_signals,
        )
    });
    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retry_state = ResponsesStreamRetryState::default();
    let mut accepted_attempt_input = prepared_input.shared_items();
    let prompt_construction_guard = turn_context
        .turn_timing_state
        .begin_local_phase(TurnLocalPhase::PromptConstruction);
    let mut prompt = build_projected_prompt_from_scaffold(
        sess.as_ref(),
        &prepared_input,
        step_context.as_ref(),
        &request_scaffold,
    );
    enforce_terminal_prompt_contract(&mut prompt, terminal_completion_only);
    let mut accepted_prompt_fingerprint = prepared_input.fingerprint();
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
        let mut attempt_progress = SamplingAttemptProgress::default();
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
            Arc::clone(&pending_tool_manifest),
            cancellation_token.child_token(),
            &mut attempt_progress,
        )
        .await
        {
            Ok(output) => {
                return Ok((
                    output,
                    retain_accepted_sampling_input(accepted_attempt_input),
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

        if !err.is_retryable() {
            return Err(err);
        }

        let retry_result = handle_retryable_response_stream_error(
            &mut retry_state,
            max_retries,
            err,
            client_session,
            &sess,
            &turn_context,
            ResponsesStreamRequest::Sampling,
            &cancellation_token,
        )
        .await;
        retry_result?;
        turn_context.turn_timing_state.record_sampling_retry();
        if attempt_progress.requires_authoritative_retry_input() {
            let history = sess.clone_history().await;
            let retry_input = prepare_sampling_prompt_for_client(
                history,
                turn_context.as_ref(),
                client_session,
                sess.services.git_workspace.as_ref(),
            )
            .await;
            let retry_fingerprint = retry_input.fingerprint();
            let prompt_is_proven_unchanged = accepted_prompt_fingerprint.is_some()
                && retry_fingerprint == accepted_prompt_fingerprint;
            if !prompt_is_proven_unchanged {
                accepted_attempt_input = retry_input.shared_items();
                prompt = build_projected_prompt_from_scaffold(
                    sess.as_ref(),
                    &retry_input,
                    step_context.as_ref(),
                    &request_scaffold,
                );
                enforce_terminal_prompt_contract(&mut prompt, terminal_completion_only);
                accepted_prompt_fingerprint = retry_fingerprint;
            }
        }
    }
}

fn retain_accepted_sampling_input(
    accepted_attempt_input: Arc<[ResponseItem]>,
) -> Arc<[ResponseItem]> {
    accepted_attempt_input
}

pub(super) async fn persist_sampling_prefix_before_dispatch(
    sess: &Session,
    manifest: Option<codex_protocol::protocol::ToolManifestItem>,
    boundary: codex_protocol::protocol::SamplingBoundaryItem,
) -> CodexResult<()> {
    let manifest_hash = manifest.as_ref().map(|manifest| manifest.hash.clone());
    let has_manifest = manifest.is_some();
    let mut prefix = Vec::with_capacity(if has_manifest { 2 } else { 1 });
    if let Some(manifest) = manifest {
        prefix.push(codex_protocol::protocol::RolloutItem::ToolManifest(
            manifest,
        ));
    }
    prefix.push(codex_protocol::protocol::RolloutItem::SamplingBoundary(
        boundary,
    ));
    sess.persist_rollout_items_ordered(prefix.as_slice())
        .await
        .map_err(|err| {
            CodexErr::Fatal(if has_manifest {
                format!("failed to order tool manifest before provider dispatch: {err}")
            } else {
                format!("failed to order sampling boundary before provider dispatch: {err}")
            })
        })?;
    if let Some(manifest_hash) = manifest_hash {
        *sess
            .queued_tool_manifest_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(manifest_hash);
    }
    Ok(())
}

fn enforce_terminal_prompt_contract(prompt: &mut Prompt, terminal_completion_only: bool) {
    if terminal_completion_only {
        if !prompt.tools.specs().is_empty() {
            prompt.tools = Arc::new(ToolSchemaArtifact::default());
        }
        prompt.digests.tools = Some(prompt.tools.digest());
        prompt.parallel_tool_calls = false;
    }
}

#[cfg(test)]
fn finalized_router_matches_exposure(
    router: &ToolRouter,
    current_identity: &ToolExposureIdentity,
) -> bool {
    router.exposure_identity() == current_identity
}

async fn finalized_router_matches_current_exposure(
    sess: &Session,
    step_context: &StepContext,
    router: &ToolRouter,
) -> bool {
    router.exposure_identity().dynamic_identity()
        == current_dynamic_tool_exposure_identity(sess, step_context).await
}

async fn current_dynamic_tool_exposure_identity(
    sess: &Session,
    step_context: &StepContext,
) -> DynamicToolExposureIdentity {
    let mcp_tool_snapshot = step_context.mcp_tool_snapshot().await;
    DynamicToolExposureIdentity {
        agent_surface_stage: agent_surface_stage(sess, step_context.turn.as_ref()),
        extension_tool_surface_revision: extension_tool_surface_revision(sess),
        mcp_tool_catalog_revision: mcp_tool_snapshot.revision,
        mcp_resources_available: mcp_tool_snapshot.resources_available,
        request_user_input_eligible: request_user_input_eligible(step_context.turn.as_ref()),
        collaboration_mode: step_context.turn.collaboration_mode.mode,
        environment_mode: EnvironmentSurfaceMode::from_count(
            step_context.environments.turn_environments.len(),
        ),
        environment_starting: !step_context.environments.starting.is_empty(),
    }
}

struct SessionPreparedRouter {
    planning_generation: u64,
    config: Arc<crate::config::Config>,
    dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
    router: Arc<ToolRouter>,
}

fn same_config_snapshot(
    left: &Arc<crate::config::Config>,
    right: &Arc<crate::config::Config>,
) -> bool {
    Arc::ptr_eq(left, right) || left.as_ref() == right.as_ref()
}

#[derive(Default)]
pub(super) struct SessionPreparedRouterCache {
    entry: std::sync::Mutex<Option<SessionPreparedRouter>>,
}

impl SessionPreparedRouterCache {
    fn get(&self, planning_generation: u64, turn_context: &TurnContext) -> Option<Arc<ToolRouter>> {
        self.entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|entry| {
                entry.planning_generation == planning_generation
                    && same_config_snapshot(&entry.config, &turn_context.config)
                    && entry.dynamic_tools.as_slice() == turn_context.dynamic_tools.as_slice()
            })
            .map(|entry| Arc::clone(&entry.router))
    }

    fn publish(
        &self,
        planning_generation: u64,
        turn_context: &TurnContext,
        router: Arc<ToolRouter>,
    ) {
        *self
            .entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SessionPreparedRouter {
            planning_generation,
            config: Arc::clone(&turn_context.config),
            dynamic_tools: turn_context.dynamic_tools.clone(),
            router,
        });
    }
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
        if sess.services.planning_generation() == planning_generation
            && finalized_router_matches_current_exposure(
                sess,
                step_context,
                prepared.router.as_ref(),
            )
            .await
        {
            turn_context.refresh_deferred_tool_capabilities(
                prepared.router.deferred_tool_capability_revisions(),
            );
            sess.prepared_tool_router.publish(
                planning_generation,
                turn_context,
                Arc::clone(&prepared.router),
            );
            trace!(
                planning_generation,
                "reused startup-prepared pending-turn router"
            );
            return Ok(prepared.router);
        }
    }

    if selected_skill_invocations.is_empty()
        && let Some(router) = sess
            .prepared_tool_router
            .get(planning_generation, turn_context)
        && sess.services.planning_generation() == planning_generation
        && finalized_router_matches_current_exposure(sess, step_context, router.as_ref()).await
    {
        turn_context
            .refresh_deferred_tool_capabilities(router.deferred_tool_capability_revisions());
        return Ok(router);
    }

    let router = built_tools(
        sess,
        step_context,
        selected_skill_invocations,
        cancellation_token,
    )
    .await?;
    if selected_skill_invocations.is_empty()
        && sess.services.planning_generation() == planning_generation
    {
        sess.prepared_tool_router
            .publish(planning_generation, turn_context, Arc::clone(&router));
    }
    Ok(router)
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
    let mcp_tool_snapshot = step_context
        .mcp_tool_snapshot()
        .or_cancel(cancellation_token)
        .await?;
    let mcp_tool_catalog_revision = mcp_tool_snapshot.revision;
    let all_mcp_tools = mcp_tool_snapshot.tools.as_slice();
    let loaded_plugins = sess
        .services
        .plugins_manager
        .plugins_for_config(&turn_context.config.plugins_config_input())
        .instrument(trace_span!("built_tools.load_plugins"))
        .await;
    let extension_tool_executors = extension_tool_executors(sess);
    let selected_skill_mcp_exposure = resolve_selected_skill_mcp_exposure(
        selected_skill_invocations,
        &loaded_plugins,
        all_mcp_tools,
    );
    for diagnostic in &selected_skill_mcp_exposure.diagnostics {
        warn!("{diagnostic}");
    }
    let exposure_identity = derive_tool_exposure_identity(
        sess,
        step_context,
        &selected_skill_mcp_exposure.direct_entrypoints,
        mcp_tool_catalog_revision,
        mcp_tool_snapshot.resources_available,
        &extension_tool_executors,
    );
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
        &selected_skill_mcp_exposure.selection,
    );
    let mcp_tools = has_mcp_servers.then_some(mcp_tool_exposure.direct_tools);
    let deferred_mcp_tools = mcp_tool_exposure.deferred_tools;
    let router = Arc::new(
        ToolRouter::try_from_context(
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
        )
        .map_err(CodexErr::InvalidRequest)?,
    );
    step_context
        .turn
        .refresh_deferred_tool_capabilities(router.deferred_tool_capability_revisions());
    Ok(router)
}

fn derive_tool_exposure_identity(
    sess: &Session,
    step_context: &StepContext,
    selected_skill_direct_mcp_entrypoints: &[DirectMcpToolEntrypoint],
    mcp_tool_catalog_revision: u64,
    mcp_resources_available: bool,
    extension_tool_executors: &[Arc<
        dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>,
    >],
) -> ToolExposureIdentity {
    let turn_context = step_context.turn.as_ref();
    let agent_surface_stage = agent_surface_stage(sess, turn_context);
    let goal_surface_state = goal_surface_state(extension_tool_executors);
    let extension_tool_surface_revision = extension_tool_surface_revision(sess);
    let tool_search_available = search_tool_enabled(turn_context);
    let request_user_input_eligible = request_user_input_eligible(turn_context);
    let environment_mode =
        EnvironmentSurfaceMode::from_count(step_context.environments.turn_environments.len());
    let environment_starting = !step_context.environments.starting.is_empty();

    ToolExposureIdentity {
        selected_skill_direct_mcp_entrypoints: selected_skill_direct_mcp_entrypoints.to_vec(),
        agent_surface_stage,
        goal_surface_state,
        extension_tool_surface_revision,
        mcp_tool_catalog_revision,
        mcp_resources_available,
        tool_search_available,
        request_user_input_eligible,
        collaboration_mode: turn_context.collaboration_mode.mode,
        environment_mode,
        environment_starting,
    }
}

fn request_user_input_eligible(turn_context: &TurnContext) -> bool {
    let available_modes = request_user_input_available_modes(turn_context.config.features.get());
    turn_context.config.experimental_request_user_input_enabled
        && !turn_context.session_source.is_non_root_agent()
        && available_modes.contains(&turn_context.collaboration_mode.mode)
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
    required_tool_terminal: Option<RequiredToolTerminal>,
    prefetched_workspace_identity: Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
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
        // A delta without a preceding ItemStarted event produces an invalid
        // client-visible item stream. Treat both out-of-phase cases as inert.
        if !self.started || self.completed {
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

#[cfg(test)]
#[instrument(level = "trace", skip_all)]
async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, InFlightToolResult>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<Option<RequiredToolTerminal>> {
    let mut completed = VecDeque::new();
    drain_in_flight_with_buffer(&mut completed, in_flight, sess, turn_context).await
}

#[instrument(level = "trace", skip_all)]
async fn drain_in_flight_with_buffer(
    completed: &mut VecDeque<InFlightToolResult>,
    in_flight: &mut FuturesOrdered<BoxFuture<'static, InFlightToolResult>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<Option<RequiredToolTerminal>> {
    let mut first_error = None;
    let mut first_required_terminal = None;
    let mut active_without_pending_passes = 0_u8;
    loop {
        let pending_tool_count = completed.len().saturating_add(in_flight.len());
        let reason = reconcile_turn_progress(
            &turn_context.turn_timing_state,
            pending_tool_count,
            &mut active_without_pending_passes,
        );
        turn_context
            .turn_timing_state
            .record_next_sample_block_reason(reason);
        trace!(?reason, pending_tool_count, "tool relay reconciliation");
        let completion = if let Some(completion) = completed.pop_front() {
            completion
        } else {
            let Some(completion) = in_flight.next().await else {
                break;
            };
            completion
        };
        let expected_execution_id = completion.timing.execution_id().clone();
        let stale_relay_error = (completion.execution_id != expected_execution_id).then(|| {
            CodexErr::Fatal(format!(
                "stale tool relay completion for {}: expected {}, received {}",
                completion.call_id, expected_execution_id.0, completion.execution_id.0,
            ))
        });
        if let Some(err) = stale_relay_error.as_ref() {
            error!("{err}");
        }
        completion
            .timing
            .mark_relay_delivery(&expected_execution_id);
        turn_context
            .turn_timing_state
            .update_tool_dispatch_lifecycle(
                &completion.call_id,
                &expected_execution_id,
                completion.timing.snapshot(tokio::time::Instant::now()),
            );
        let (response_input, required_terminal, completion_error) = match stale_relay_error {
            Some(err) => {
                let response_input = ToolCallRuntime::failure_response_for_message(
                    &completion.call,
                    err.to_string(),
                );
                (response_input, None, Some(err))
            }
            None => match completion.result {
                Ok(completion) => (completion.response, completion.required_terminal, None),
                Err(err) => {
                    error!("in-flight tool future failed during drain: {err}");
                    let response_input = ToolCallRuntime::failure_response_for_message(
                        &completion.call,
                        err.to_string(),
                    );
                    (response_input, None, Some(err))
                }
            },
        };
        let mut response_items = vec![response_input.into()];
        response_items.extend(
            turn_context
                .take_post_tool_contexts(&completion.call_id)
                .await,
        );
        if let Err(err) = sess
            .record_conversation_items_ordered(&turn_context, &response_items)
            .await
        {
            let interrupt_terminal = sess
                .active_turn
                .lock()
                .await
                .as_ref()
                .and_then(|active| active.terminal.clone())
                .filter(|terminal| terminal.interrupt_pending())
                .map(|terminal| {
                    let generation = terminal.wake_generation_id();
                    (terminal, generation)
                });
            if let Some((terminal, generation)) = interrupt_terminal {
                let _ = terminal.mark_interrupt_persistence_failed(&generation);
                return Err(CodexErr::Fatal(format!(
                    "failed to append interrupted tool output in order: {err}"
                )));
            }
            return Err(CodexErr::Fatal(format!(
                "failed to durably append tool output in order: {err}"
            )));
        }
        for response_item in &response_items {
            mark_thread_memory_mode_polluted_if_external_context(
                sess.as_ref(),
                turn_context.as_ref(),
                response_item,
            )
            .await;
        }
        record_reconciliation(
            &turn_context.turn_timing_state,
            completed.len().saturating_add(in_flight.len()),
            &mut active_without_pending_passes,
            if completion_error.is_some() {
                "relay error delivery"
            } else {
                "relay delivery"
            },
        );
        if first_error.is_none() {
            first_error = completion_error;
        }
        if first_required_terminal.is_none() {
            first_required_terminal = required_terminal;
        }
    }
    first_error.map_or(Ok(first_required_terminal), Err)
}

pub(crate) fn reconcile_turn_progress(
    turn_timing_state: &TurnTimingState,
    pending_tool_count: usize,
    active_without_pending_passes: &mut u8,
) -> NextSampleBlockReason {
    let context = turn_timing_state.lifecycle_context();
    if pending_tool_count == 0 && context.active_tool_count > 0 {
        *active_without_pending_passes = active_without_pending_passes.saturating_add(1);
        trace!(
            active_without_pending_passes = *active_without_pending_passes,
            active_tool_count = context.active_tool_count,
            "active tool lifecycle is awaiting a later reconciliation pass"
        );
    } else {
        *active_without_pending_passes = 0;
    }

    if context.active_tool_count > 0 {
        NextSampleBlockReason::WaitingForTool
    } else if context.relay_queue_depth > 0 {
        NextSampleBlockReason::WaitingForDelivery
    } else if context.parallel_gate_waiter_count > 0 || context.sampling_gate_waiter_count > 0 {
        NextSampleBlockReason::WaitingForGate
    } else if context.process_output_waiter_count > 0 {
        NextSampleBlockReason::WaitingForProcessCleanup
    } else if pending_tool_count > 0 {
        NextSampleBlockReason::WaitingForTool
    } else {
        NextSampleBlockReason::ReadyToSample
    }
}

fn hold_sampling_readiness_for_ordered_prefix(
    reason: NextSampleBlockReason,
) -> NextSampleBlockReason {
    if reason == NextSampleBlockReason::ReadyToSample {
        // The provider request is not dispatchable until its sampling boundary (and, when
        // present, the matching tool manifest) has joined the ordered rollout. Keep the timing
        // state blocked until that append and context binding both succeed.
        NextSampleBlockReason::WaitingForDelivery
    } else {
        reason
    }
}

pub(crate) fn reconcile_turn_progress_event(
    turn_timing_state: &TurnTimingState,
    pending_tool_count: usize,
    event: &'static str,
) -> NextSampleBlockReason {
    let mut active_without_pending_passes = 0;
    record_reconciliation(
        turn_timing_state,
        pending_tool_count,
        &mut active_without_pending_passes,
        event,
    )
}

fn record_reconciliation(
    turn_timing_state: &TurnTimingState,
    pending_tool_count: usize,
    active_without_pending_passes: &mut u8,
    event: &'static str,
) -> NextSampleBlockReason {
    let reason = reconcile_turn_progress(
        turn_timing_state,
        pending_tool_count,
        active_without_pending_passes,
    );
    turn_timing_state.record_next_sample_block_reason(reason);
    trace!(
        ?reason,
        pending_tool_count, event, "turn progress reconciled"
    );
    reason
}

struct SamplingGateWaiterGuard {
    turn_timing_state: Arc<TurnTimingState>,
}

impl SamplingGateWaiterGuard {
    fn new(turn_timing_state: Arc<TurnTimingState>) -> Self {
        turn_timing_state.adjust_sampling_gate_waiters(1);
        Self { turn_timing_state }
    }
}

impl Drop for SamplingGateWaiterGuard {
    fn drop(&mut self) {
        self.turn_timing_state.adjust_sampling_gate_waiters(-1);
    }
}

fn start_eager_tool_future(future: InFlightToolCall) -> BoxFuture<'static, InFlightToolResult> {
    // Dropping a raw Tokio JoinHandle detaches its task. Keeping the abort-on-drop
    // wrapper inside the ordered future makes collection teardown abort eager work.
    let call_id = future.call_id.clone();
    let call = future.call.clone();
    let execution_id = future.execution_id.clone();
    let timing = Arc::clone(&future.timing);
    let handle = AbortOnDropHandle::new(tokio::spawn(future.into_future()));
    Box::pin(async move {
        match handle.await {
            Ok(result) => result,
            Err(err) => {
                timing.mark_relay_enqueue();
                InFlightToolResult {
                    call,
                    call_id,
                    execution_id,
                    timing,
                    result: Err(CodexErr::Fatal(format!("eager tool task failed: {err}"))),
                }
            }
        }
    })
}

/// How the provider response tail ended.
///
/// Deferred tools are admitted only on [`ResponseTailOutcome::SuccessfulTail`]. A terminal
/// stream error or a cancellation retires them without invoking their handlers, so a
/// response the provider never completed cannot produce a side effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResponseTailOutcome {
    SuccessfulTail,
    TerminalError,
    Cancelled,
}

impl ResponseTailOutcome {
    fn unexecuted_message(self) -> Option<&'static str> {
        match self {
            Self::SuccessfulTail => None,
            Self::TerminalError => Some(
                "This tool was not executed: the model response stream failed before it closed.",
            ),
            Self::Cancelled => {
                Some("This tool was not executed: the turn was interrupted before it started.")
            }
        }
    }
}

/// Wakes deferred tool calls once and tells them whether they may run.
#[derive(Clone)]
pub(crate) struct ResponseTailSignal {
    closed: CancellationToken,
    outcome: Arc<OnceLock<ResponseTailOutcome>>,
}

impl ResponseTailSignal {
    fn new() -> Self {
        Self {
            closed: CancellationToken::new(),
            outcome: Arc::new(OnceLock::new()),
        }
    }

    /// Publishes the tail outcome, then wakes every deferred call.
    ///
    /// The outcome is written before the wake so a woken call always observes it. Only the
    /// first close is authoritative.
    fn close(&self, outcome: ResponseTailOutcome) {
        let _ = self.outcome.set(outcome);
        self.closed.cancel();
    }

    async fn wait(&self) -> ResponseTailOutcome {
        self.closed.cancelled().await;
        self.outcome
            .get()
            .copied()
            // A wake without a published outcome can only come from teardown.
            .unwrap_or(ResponseTailOutcome::Cancelled)
    }
}

fn defer_tool_future_until_response_tail(
    future: InFlightToolCall,
    response_tail: ResponseTailSignal,
) -> BoxFuture<'static, InFlightToolResult> {
    Box::pin(async move {
        match response_tail.wait().await.unexecuted_message() {
            // The tail never closed successfully, so this call was never admitted. Retire it
            // with a model-visible output instead of running its handler.
            Some(message) => future.into_unexecuted_result(message.to_string()),
            None => future.into_future().await,
        }
    })
}

fn tool_argument_diff_target(item: &ResponseItem) -> Option<(String, ToolName)> {
    match item {
        ResponseItem::CustomToolCall {
            call_id,
            name,
            namespace,
            ..
        }
        | ResponseItem::FunctionCall {
            call_id,
            name,
            namespace,
            ..
        } => Some((
            call_id.clone(),
            ToolName::new(namespace.clone(), name.as_str()),
        )),
        _ => None,
    }
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
    pending_tool_manifest: Arc<Mutex<Option<codex_protocol::protocol::ToolManifestItem>>>,
    cancellation_token: CancellationToken,
    attempt_progress: &mut SamplingAttemptProgress,
) -> CodexResult<SamplingRequestResult> {
    let mut active_without_pending_passes = 0_u8;
    let next_sample_reason = hold_sampling_readiness_for_ordered_prefix(reconcile_turn_progress(
        &turn_context.turn_timing_state,
        0,
        &mut active_without_pending_passes,
    ));
    turn_context
        .turn_timing_state
        .record_next_sample_block_reason(next_sample_reason);
    trace!(?next_sample_reason, "sampling admission reconciliation");
    if !sess.has_reference_context_item().await {
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
        turn_context
            .turn_timing_state
            .record_next_sample_block_reason(NextSampleBlockReason::WaitingForGate);
        let wait_started_at_ms = turn_context.turn_timing_state.monotonic_offset_ms();
        let expected_generation = terminal.wake_generation_id();
        let _sampling_waiter =
            SamplingGateWaiterGuard::new(Arc::clone(&turn_context.turn_timing_state));
        let wake = terminal
            .wait_for_interrupt_resolution(&expected_generation)
            .await;
        let wait_finished_at_ms = turn_context.turn_timing_state.monotonic_offset_ms();
        turn_context
            .turn_timing_state
            .record_pending_tool_timer_wait(ToolLifecycleTimerWait {
                wait_kind: "sampling_interrupt_resolution".to_string(),
                requested_timeout_ms: None,
                effective_timeout_ms: Some(wait_finished_at_ms.saturating_sub(wait_started_at_ms)),
                deadline_at_ms: None,
                wake_reason: if wake == TerminalWakeResult::Applied {
                    ToolLifecycleWakeReason::Completed
                } else {
                    ToolLifecycleWakeReason::Retry
                },
                sequence: 0,
            });
        record_reconciliation(
            &turn_context.turn_timing_state,
            0,
            &mut active_without_pending_passes,
            "sampling interrupt resolution",
        );
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
            .begin_startup_prewarm_wait_outside_preparation(preparation_timing_guard);
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
    startup_snapshot.trace_frozen_at_first_model_send();
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
    let boundary_turn_timing = Arc::clone(&turn_context.turn_timing_state);
    let bound_context_attempts = Arc::new(Mutex::new(HashSet::new()));
    let boundary_bound_context_attempts = Arc::clone(&bound_context_attempts);
    let boundary_pending_tool_manifest = Arc::clone(&pending_tool_manifest);
    let attempt_prepared: AttemptPreparedCallback = Arc::new(move |identity| {
        let sess = Arc::clone(&boundary_session);
        let turn_id = boundary_turn_id.clone();
        let turn_timing_state = Arc::clone(&boundary_turn_timing);
        let bound_context_attempts = Arc::clone(&boundary_bound_context_attempts);
        let pending_tool_manifest = Arc::clone(&boundary_pending_tool_manifest);
        Box::pin(async move {
            let terminal = sess
                .active_turn
                .lock()
                .await
                .as_ref()
                .and_then(|active| active.terminal.clone());
            let sampling_admission = if let Some(terminal) = terminal.as_ref() {
                turn_timing_state
                    .record_next_sample_block_reason(NextSampleBlockReason::WaitingForGate);
                let wait_started_at_ms = turn_timing_state.monotonic_offset_ms();
                let sampling_waiter = SamplingGateWaiterGuard::new(Arc::clone(&turn_timing_state));
                let Some(admission) = terminal.acquire_sampling_admission().await else {
                    let expected_generation = terminal.wake_generation_id();
                    let wake = terminal
                        .wait_for_interrupt_resolution(&expected_generation)
                        .await;
                    let wait_finished_at_ms = turn_timing_state.monotonic_offset_ms();
                    turn_timing_state.record_pending_tool_timer_wait(ToolLifecycleTimerWait {
                        wait_kind: "sampling_gate_acquisition_and_resolution".to_string(),
                        requested_timeout_ms: None,
                        effective_timeout_ms: Some(
                            wait_finished_at_ms.saturating_sub(wait_started_at_ms),
                        ),
                        deadline_at_ms: None,
                        wake_reason: if wake == TerminalWakeResult::Applied {
                            ToolLifecycleWakeReason::Completed
                        } else {
                            ToolLifecycleWakeReason::Retry
                        },
                        sequence: 0,
                    });
                    reconcile_turn_progress_event(&turn_timing_state, 0, "sampling gate fenced");
                    return if terminal.interrupt_persistence_failed() {
                        Err(CodexErr::Fatal(
                            "interrupted request_user_input output was not durably persisted"
                                .to_string(),
                        ))
                    } else {
                        Err(CodexErr::TurnAborted)
                    };
                };
                drop(sampling_waiter);
                let wait_finished_at_ms = turn_timing_state.monotonic_offset_ms();
                turn_timing_state.record_pending_tool_timer_wait(ToolLifecycleTimerWait {
                    wait_kind: "sampling_gate_acquisition".to_string(),
                    requested_timeout_ms: None,
                    effective_timeout_ms: Some(
                        wait_finished_at_ms.saturating_sub(wait_started_at_ms),
                    ),
                    deadline_at_ms: None,
                    wake_reason: ToolLifecycleWakeReason::Completed,
                    sequence: 0,
                });
                reconcile_turn_progress_event(&turn_timing_state, 0, "sampling gate acquired");
                Some(admission)
            } else {
                None
            };
            let manifest = pending_tool_manifest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            persist_sampling_prefix_before_dispatch(
                sess.as_ref(),
                manifest,
                SamplingBoundaryItem {
                    sampling_request_id: identity.sampling_request_id.clone(),
                    physical_attempt_id: identity.physical_attempt_id.clone(),
                    turn_id: Some(turn_id),
                    unresolved_context: true,
                },
            )
            .await?;
            if sess
                .bind_context_baseline_candidate(
                    &identity.sampling_request_id,
                    &identity.physical_attempt_id,
                )
                .await
            {
                bound_context_attempts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert((
                        identity.sampling_request_id.clone(),
                        identity.physical_attempt_id.clone(),
                    ));
            }
            turn_timing_state.record_next_sample_block_reason(NextSampleBlockReason::ReadyToSample);
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
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    let mut completed_in_flight = VecDeque::new();
    let response_tail = ResponseTailSignal::new();
    let response_item_recorder = OrderedResponseItemRecorder::default();
    let mut earlier_tool_calls_eligible = true;
    let mut all_tool_calls_eager_read_eligible = true;
    let mut continuation_workspace_prefetch = None;
    let mut needs_follow_up = false;
    let mut tool_result_continuation = false;
    let mut server_end_turn_false = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut active_tool_argument_diff_consumer: Option<(
        String,
        Box<dyn ToolArgumentDiffConsumer>,
    )> = None;
    let mut context_baseline_committed = false;
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
    // S1: only a contributor that may rewrite assistant text forces buffering. Contributors
    // that just annotate non-text fields keep true streaming, and buffered items still replay
    // their finalized text as a delta before completion.
    let defer_streamed_turn_items_for_contributors = sess
        .services
        .extensions
        .turn_item_contributors()
        .iter()
        .any(|contributor| contributor.mutates_assistant_text());
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
        let stream_event = if in_flight.is_empty() {
            Either::Left(
                stream
                    .next()
                    .instrument(trace_span!(parent: &handle_responses, "receiving"))
                    .or_cancel(&cancellation_token)
                    .await,
            )
        } else {
            tokio::select! {
                event = stream
                    .next()
                    .instrument(trace_span!(parent: &handle_responses, "receiving"))
                    .or_cancel(&cancellation_token) => Either::Left(event),
                completion = in_flight.next() => Either::Right(completion),
            }
        };
        drop(model_stream_wait_timing_guard);
        let stream_event = match stream_event {
            Either::Left(stream_event) => stream_event,
            Either::Right(Some(completion)) => {
                // Poll eager work while the provider is still streaming, but
                // defer model-visible delivery until the response has closed.
                // FuturesOrdered makes this buffer provider-call ordered.
                completed_in_flight.push_back(completion);
                continue;
            }
            Either::Right(None) => continue,
        };
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

        if !context_baseline_committed && let Some(identity) = attempt_identity.as_ref() {
            let context_baseline_was_bound = bound_context_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|(sampling_request_id, physical_attempt_id)| {
                    sampling_request_id == &identity.sampling_request_id
                        && physical_attempt_id == &identity.physical_attempt_id
                });
            if !context_baseline_was_bound {
                // Unchanged world state does not stage a context candidate. Avoid taking the
                // session-state lock on the first response event when there is nothing to commit.
                context_baseline_committed = true;
            } else {
                match sess
                    .commit_context_baseline_candidate(
                        &identity.sampling_request_id,
                        &identity.physical_attempt_id,
                    )
                    .await
                {
                    Ok(true) => context_baseline_committed = true,
                    Ok(false) => {
                        sess.mark_context_baseline_unknown().await;
                        client_session.invalidate_provider_history_inheritance(
                            "prepared context did not match the provider-accepted attempt",
                        );
                        break Err(CodexErr::Fatal(
                            "provider accepted a sampling attempt without a matching prepared context"
                                .to_string(),
                        ));
                    }
                    Err(err) => {
                        sess.mark_context_baseline_unknown().await;
                        client_session.invalidate_provider_history_inheritance(
                            "authoritative context persistence failed after provider acceptance",
                        );
                        break Err(CodexErr::Fatal(format!(
                            "provider accepted sampling attempt but authoritative context persistence failed: {err}"
                        )));
                    }
                }
            }
        }

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(mut item) => {
                attempt_progress.accepted_output = true;
                assign_missing_streamed_response_item_id(&mut item, active_item.as_ref());
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
                    response_item_recorder: response_item_recorder.clone(),
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
                    all_tool_calls_eager_read_eligible &= output_result.eager_read_eligible;
                    if output_result.eager_read_eligible {
                        in_flight.push_back(start_eager_tool_future(tool_future));
                    } else {
                        in_flight.push_back(defer_tool_future_until_response_tail(
                            tool_future,
                            response_tail.clone(),
                        ));
                    }
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
                tool_result_continuation |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(mut item) => {
                assign_missing_streamed_response_item_id(&mut item, /*active_item*/ None);
                if let Some((call_id, tool_name)) = tool_argument_diff_target(&item) {
                    active_tool_argument_diff_consumer = tool_runtime
                        .create_diff_consumer(&tool_name)
                        .map(|consumer| (call_id, consumer));
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
                if needs_follow_up && !in_flight.is_empty() && all_tool_calls_eager_read_eligible {
                    let history = sess.clone_history().await;
                    continuation_workspace_prefetch = start_continuation_workspace_prefetch(
                        &history,
                        &turn_diff_tracker,
                        Arc::clone(&sess.services.git_workspace),
                        turn_context.config.cwd.clone(),
                    )
                    .await;
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
    // FuturesOrdered polls every inserted slot while streaming. Open deferred slots only
    // after the provider response closes so non-eager tools cannot begin execution during the
    // response tail, and only when that close was successful: a terminal stream error or a
    // cancellation retires them unexecuted rather than running a handler for a response the
    // provider never completed. Tools already admitted before the error are not undone.
    let tail_outcome = match &outcome {
        Ok(_) => ResponseTailOutcome::SuccessfulTail,
        Err(CodexErr::TurnAborted | CodexErr::Interrupted) => ResponseTailOutcome::Cancelled,
        Err(_) => ResponseTailOutcome::TerminalError,
    };
    response_tail.close(tail_outcome);
    drop(sampling_timing_guard);

    flush_assistant_text_segments_all(
        &sess,
        &turn_context,
        plan_mode_state.as_mut(),
        &mut assistant_message_stream_parsers,
    )
    .await;
    response_item_recorder.flush().await;

    let tool_blocking_timing_guard = if in_flight.is_empty() && completed_in_flight.is_empty() {
        None
    } else {
        Some(turn_context.turn_timing_state.begin_tool_blocking())
    };
    let required_tool_terminal = drain_in_flight_with_buffer(
        &mut completed_in_flight,
        &mut in_flight,
        sess.clone(),
        turn_context.clone(),
    )
    .await;
    drop(tool_blocking_timing_guard);
    let generation_workspace_evidence = tool_runtime.flush_workspace_evidence_generation().await;
    let required_tool_terminal = required_tool_terminal?;

    let terminal = {
        let active_turn = sess.active_turn.lock().await;
        active_turn
            .as_ref()
            .and_then(|active| active.terminal.clone())
    };
    if let Some(terminal) = terminal
        && terminal.interrupt_pending()
    {
        let generation = terminal.wake_generation_id();
        if let Err(err) = sess.flush_rollout().await {
            let _ = terminal.mark_interrupt_persistence_failed(&generation);
            return Err(CodexErr::Fatal(format!(
                "failed to durably flush interrupted request_user_input output: {err}"
            )));
        }
        if terminal.mark_interrupt_output_durable(&generation) == TerminalWakeResult::Stale {
            return Err(CodexErr::TurnAborted);
        }
        return Err(CodexErr::TurnAborted);
    }

    // A tool result guarantees another request in this turn. A later assistant
    // item in the same response must not defer already queued mailbox input.
    if tool_result_continuation && required_tool_terminal.is_none() {
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
            tool_exposure_revision: turn_context.deferred_tool_activation_revision(),
        }
    };
    let prefetched_workspace_identity =
        match generation_workspace_evidence.prefetched_workspace_identity {
            Some(identity) => Some(identity),
            None => match continuation_workspace_prefetch {
                Some((baseline_mutation_revision, handle))
                    if continuation_workspace_prefetch_is_current(
                        baseline_mutation_revision,
                        settled_state.mutation_revision,
                        false,
                    ) =>
                {
                    handle.await.ok()
                }
                _ => None,
            },
        };
    let outcome = outcome.map(|result| SamplingRequestResult {
        needs_follow_up: result.needs_follow_up,
        last_agent_message: result.last_agent_message,
        settled_state,
        tool_result_continuation: result.tool_result_continuation,
        server_end_turn_false: result.server_end_turn_false,
        required_tool_terminal,
        prefetched_workspace_identity,
    });

    if should_emit_turn_diff {
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.take_unified_diff_if_changed()
        };
        if let Some(unified_diff) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    if let Some(etag) = latest_models_etag {
        let models_manager = Arc::clone(&sess.services.models_manager);
        let http_client_factory = turn_context.config.http_client_factory();
        drop(tokio::spawn(async move {
            models_manager.notify_etag(etag, http_client_factory).await;
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
