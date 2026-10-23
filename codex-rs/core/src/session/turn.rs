use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::Weak;
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
use crate::context::is_startup_contextual_user_fragment;
use crate::context::world_state::WorldState;
use crate::context_manager::ContextManager;
use crate::context_manager::PreparedPromptInput;
use crate::feedback_tags;
use crate::hook_runtime::emit_hook_stop_reason;
use crate::hook_runtime::inspect_pending_input;
use crate::hook_runtime::prepare_additional_context_items;
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
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::build_plugin_injections;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_retry::AccountChangeWait;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_retryable_response_stream_error;
use crate::responses_retry::wait_for_account_change;
use crate::session::EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
use crate::session::PreparedContextUpdate;
use crate::session::PreviousTurnSettings;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_execution::AuthoritativeWaitResolution;
use crate::session::turn_execution::ContinuationDisposition;
use crate::session::turn_execution::GenerationRequestDisposition;
use crate::session::turn_execution::SamplingConvergenceDecision;
use crate::session::turn_execution::SamplingGenerationDisposition;
use crate::session::turn_execution::SamplingRequestSettledState;
use crate::session::turn_execution::SamplingRequestSignalCollector;
use crate::session::turn_execution::TurnExecutionControl;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::stable_context::StableContextKind;
use crate::stable_context::StableContextTarget;
use crate::stable_context::filter_unchanged_stable_context_items;
use crate::stable_context::mark_trusted_stable_context_item;
use crate::state::SamplingAdmission;
use crate::state::TerminalWakeResult;
use crate::stream_events_utils::FinalizedAssistantOutput;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::InFlightToolCall;
use crate::stream_events_utils::InFlightToolResult;
use crate::stream_events_utils::OrderedResponseItemRecorder;
use crate::stream_events_utils::TurnItemContributorPolicy;
use crate::stream_events_utils::finalize_non_tool_response_item;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
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
use crate::tools::router::extension_tool_surface_revision;
use crate::tools::router::extension_tool_surface_snapshot;
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
use codex_core_plugins::PluginsConfigInput;
use codex_core_plugins::RecommendedPluginCandidatesInput;
use codex_core_skills::injection::InjectedHostSkillPrompts;
use codex_core_skills::injection::PlannedSkillInjections;
use codex_core_skills::injection::plan_skill_injections_with_resolved;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputEnvironment;
use codex_features::Feature;
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
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentReasoningSectionBreakEvent;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
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
use codex_tools::request_user_input_available_modes;
use codex_utils_stream_parser::AssistantTextChunk;
use codex_utils_stream_parser::AssistantTextStreamParser;
use codex_utils_stream_parser::ProposedPlanSegment;
use codex_utils_stream_parser::extract_proposed_plan_text;
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
    git_workspace: &crate::git_workspace::GitWorkspaceCache,
) -> PreparedPromptInput {
    with_sampling_workspace_identity(
        history,
        turn_context,
        git_workspace,
        |history, workspace_identity| {
            prepare_sampling_prompt_with_workspace_identity(
                history,
                turn_context,
                workspace_identity,
                git_workspace,
            )
        },
    )
    .await
}

/// Capture once for synchronous planning of one request. The callback cannot
/// await work or retain a borrowed identity for another request. Its history
/// rewrites must preserve workspace evidence requirements (for example, trimming
/// tool-search schemas); new evidence or a changed environment needs a new call.
pub(crate) async fn with_sampling_workspace_identity<R>(
    history: ContextManager,
    turn_context: &TurnContext,
    git_workspace: &crate::git_workspace::GitWorkspaceCache,
    prepare: impl FnOnce(ContextManager, Option<&crate::git_workspace::WorkspaceEvidenceIdentity>) -> R,
) -> R {
    let workspace_identity = if history.requires_workspace_evidence_validation() {
        if history.can_use_root_only_workspace_identity()
            && !turn_context
                .environments
                .primary()
                .is_some_and(|selected| selected.environment.is_remote())
        {
            git_workspace
                .workspace_root_identity(turn_context.config.cwd.as_path())
                .await
        } else {
            git_workspace
                .workspace_evidence_for_turn(turn_context, turn_context.config.cwd.as_path())
                .await
                .identity
        }
    } else {
        None
    };
    prepare(history, workspace_identity.as_ref())
}

pub(crate) fn prepare_sampling_prompt_with_workspace_identity(
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

type ContinuationWorkspacePrefetch = (
    u64,
    AbortOnDropHandle<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
);

async fn start_continuation_workspace_prefetch(
    history: &ContextManager,
    turn_diff_tracker: &Arc<tokio::sync::Mutex<TurnDiffTracker>>,
    git_workspace: Arc<crate::git_workspace::GitWorkspaceCache>,
    cwd: codex_utils_absolute_path::AbsolutePathBuf,
    environments: crate::environment_selection::TurnEnvironmentSnapshot,
) -> Option<ContinuationWorkspacePrefetch> {
    if !history.requires_workspace_evidence_validation() {
        return None;
    }
    // Do not launch a full scan that the projection cannot use. Recheck the
    // final history in prepare_sampling_prompt_for_client: later calls or
    // recovered evidence may require a full identity after all.
    if history.can_use_root_only_workspace_identity()
        && !environments
            .primary()
            .is_some_and(|selected| selected.environment.is_remote())
    {
        return None;
    }
    let baseline_mutation_revision = turn_diff_tracker.lock().await.current_mutation_revision();
    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        git_workspace
            .workspace_evidence_for_environment(&environments, cwd.as_path(), cwd.as_path())
            .await
            .identity
    }));
    Some((baseline_mutation_revision, handle))
}

async fn reuse_workspace_prefetch_for_local_compaction(
    turn_context: &TurnContext,
    turn_diff_tracker: &SharedTurnDiffTracker,
    pending: &mut Option<ContinuationWorkspacePrefetch>,
    identity: &mut Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    // Remote compaction does not use workspace evidence. Leave its capture in
    // flight for recovery rather than adding a dispatch barrier.
    if identity.is_some()
        || should_use_remote_compact_task(
            turn_context.provider.info(),
            turn_context.config.compact_prompt.as_deref(),
        )
    {
        return Ok(());
    }
    if let Some((baseline, handle)) = pending.take() {
        let revision = turn_diff_tracker.lock().await.current_mutation_revision();
        if continuation_workspace_prefetch_is_current(baseline, revision, false) {
            *identity = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                result = handle => result.ok(),
            };
        }
    }
    Ok(())
}

async fn finish_stopped_session_start(
    sess: &Session,
    turn_context: &TurnContext,
    input: Vec<TurnInput>,
) -> TurnTaskResult {
    let defer_pending_input = !input.is_empty();
    let owned = if let Some(state) = sess
        .input_queue
        .turn_state_for_sub_id(&sess.active_turn, &turn_context.sub_id)
        .await
    {
        !state.lock().await.input_in_flight.is_empty()
    } else {
        false
    };
    // Installed startup already has a recovery copy. Direct/test callers do
    // not, so preserve their prior transfer behavior without duplicating it.
    if !owned {
        sess.input_queue
            .restore_transferred_startup_input(input)
            .await;
    }
    TurnTaskResult {
        defer_pending_input,
        ..TurnTaskResult::default()
    }
}

pub(crate) fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    input: Vec<TurnInput>,
    prewarmed_client_session: Option<ModelClientSession>,
    logical_generation_budget: &mut LogicalGenerationBudget,
    cancellation_token: CancellationToken,
) -> impl std::future::Future<Output = CodexResult<TurnTaskResult>> + '_ {
    // Return pointer-sized storage so callers do not reserve a second copy of
    // this state machine in their own polling frames before Box::pin runs.
    Box::pin(async move {
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let agent_turn_preparation = sess
            .services
            .agent_control
            .task_coordinator()
            .prepare_legacy_agent_turn(&turn_context.session_source, sess.thread_id)
            .await;
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if let Err(error) = agent_turn_preparation {
            let err = CodexErr::Fatal(format!(
                "Failed to prepare the agent's follow-up task: {error}"
            ));
            sess.emit_turn_error_lifecycle(turn_context.as_ref(), err.to_codex_protocol_error())
                .await;
            sess.track_turn_codex_error(turn_context.as_ref(), &err);
            sess.send_event(
                turn_context.as_ref(),
                EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
            )
            .await;
            return Err(err);
        }
        turn_context.agent_task_binding.get_or_init(|| {
            sess.services
                .agent_control
                .task_coordinator()
                .binding_for_source(&turn_context.session_source)
                .filter(|binding| {
                    binding.thread_id.as_deref() == Some(sess.thread_id.to_string().as_str())
                })
        });
        let mut finalized_mutation_revision = None;
        let mut seen_stop_repairs = Vec::new();
        let mut completion_evidence = None;
        let mut provider_overflow_recovery_attempted = false;
        // How the overflow arm last relieved the context, until a request
        // completes. A summary leaves no smaller history to summarize.
        let mut unsampled_overflow_relief: Option<ContextPressureRelief> = None;
        let mut final_answer_compaction_deferred = false;
        if run_pending_session_start_hooks(&sess, &turn_context).await {
            return Ok(finish_stopped_session_start(&sess, &turn_context, input).await);
        }
        let inspected = inspect_inputs(&sess, &turn_context, &input).await;
        if let Some(state) = sess
            .input_queue
            .turn_state_for_sub_id(&sess.active_turn, &turn_context.sub_id)
            .await
        {
            let mut state = state.lock().await;
            if !state.input_in_flight.is_empty() {
                state.input_in_flight = inspected.items.iter().cloned().collect();
            }
        }
        let input = inspected.items;
        if inspected.outcome.should_stop {
            record_inspected_inputs(&sess, &turn_context, &input).await?;
            return Ok(TurnTaskResult::default());
        }
        let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(
            TurnDiffTracker::with_environment_display_roots([]),
        ));
        let mut pending_workspace_prefetch = start_continuation_workspace_prefetch(
            &sess.clone_history().await,
            &turn_diff_tracker,
            Arc::clone(&sess.services.git_workspace),
            turn_context.config.cwd.clone(),
            turn_context.environments.clone(),
        )
        .await;
        let mut preparation_timing_guard = None;
        let mut client_session =
            prewarmed_client_session.unwrap_or_else(|| sess.services.model_client.new_session());
        // Startup-capable clients already have a speculative owner until the first send.
        // Desktop attestation intentionally disables that path, but is available now.
        // First-send handoff cuts off an unfinished prewarm, so only a finished one owns
        // the first socket. Otherwise preconnect now rather than dial serially at dispatch.
        let startup_owns_transport = sess.services.model_client.startup_websocket_enabled()
            && sess
                .state
                .lock()
                .await
                .startup_prewarm
                .as_ref()
                .is_some_and(|prewarm| prewarm.is_finished());
        if !startup_owns_transport {
            let metadata = turn_context.turn_metadata_state.to_responses_metadata(
                sess.installation_id.clone(),
                sess.current_window_id().await,
                CodexResponsesRequestKind::Turn,
            );
            client_session
                .start_websocket_preconnect(
                    &turn_context.model_info,
                    service_tier_for_sampling(&sess, &turn_context).await,
                    &turn_context.session_telemetry,
                    &metadata,
                )
                .await;
        }
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
            let mut pending_turn_plan = match Box::pin(stabilize_pending_turn_plan(
                &sess,
                &turn_context,
                &input,
                &mut client_session,
                &mut planning_iterations,
                &mut completed_mcp_effect,
                &cancellation_token,
            ))
            .await
            {
                Ok(plan) => plan,
                Err(err) => break Err(err),
            };
            let prepared_context_update =
                take_stabilized_context_update(&mut pending_turn_plan.prepared_context_update)?;
            let (world_state, display_roots) = tokio::join!(
                time_planning_step("context_commit", || sess
                    .compare_and_record_context_updates(
                        prepared_context_update,
                        pending_turn_plan.planning_generation,
                    )),
                time_planning_step("diff_roots", || turn_diff_display_roots(
                    sess.as_ref(),
                    turn_context.as_ref()
                )),
            );
            let world_state = match world_state {
                Ok(world_state) => world_state,
                Err(error) => break Err(CodexErr::from(error)),
            };
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
                return finish_pending_turn_planning_failure(&sess, &turn_context, &input, err)
                    .await;
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

        let mut can_drain_pending_input = input.is_empty();
        record_inspected_inputs(&sess, &turn_context, &input).await?;

        sess.merge_connector_selection(explicitly_enabled_connectors.clone())
            .await;
        sess.set_previous_turn_settings(Some(PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: turn_context.model_info.comp_hash.clone(),
        }))
        .await;
        if !injection_items.is_empty() {
            sess.record_conversation_items(&turn_context, &injection_items)
                .await?;
        }

        track_turn_resolved_config_analytics(&sess, &turn_context, &input).await;

        let mut last_agent_message: Option<String> = None;
        let mut surfaced_result: Option<SurfacedToolResult> = None;
        let mut output_state = TurnOutputState::default();
        let mut accepted_final_message = None;
        let mut completion_assessment: Option<codex_protocol::protocol::TurnCompletionAssessment>;
        let mut stop_hook_active = false;
        let mut pending_continuation_cause = None;
        let mut pending_generation_request: Option<GenerationRequestDisposition> = None;
        let mut missing_answer_terminal_repair_revision: Option<String> = None;
        let mut completion_notice_key = None;
        let mut prefetched_workspace_identity: Option<
            Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
        > = None;
        let mut has_started_generation = false;
        let mut logical_generation_ordinal = 0_u32;
        let mut generation_budget_error_reported = false;
        // Mid-turn context-limit compactions (or retirements) in this turn; they
        // bound how much a spawned agent reads before it reports to its parent.
        let mut context_pressure_events = 0_u32;
        let mut subagent_context_budget_reached = false;
        let mut defer_pending_input = false;
        // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
        // many turns, from the perspective of the user, it is a single turn.
        turn_diff_tracker
            .lock()
            .await
            .set_environment_display_roots(display_roots);
        let kd4_runtime = turn_context.config.features.enabled(Feature::Kd4Runtime);
        sess.state
            .lock()
            .await
            .history
            .rehydrate_read_replays(&sess.services.path_replays);
        let mut turn_execution =
            TurnExecutionControl::new_with_timing(Arc::clone(&turn_context.turn_timing_state))
                .with_session_path_replays(Arc::clone(&sess.services.path_replays))
                .with_session_validation_uncertainty(Arc::clone(
                    &sess.services.validation_uncertainty,
                ))
                .for_validation_turn(&turn_context.sub_id)
                .with_active_plan(if turn_context.collaboration_mode.mode == ModeKind::Plan {
                    None
                } else {
                    sess.services.plan_store.execution_snapshot().await
                });
        turn_execution.rehydrate_argument_failures(sess.state.lock().await.history.raw_items());
        // A root session publishes its request and edits while this turn runs, so
        // concurrent sessions in the checkout do not mistake them for their own.
        let mut live_session = (!matches!(
            turn_context.session_source,
            SessionSource::SubAgent(_) | SessionSource::Internal(_)
        ))
        .then(|| turn_input_first_text(&input))
        .flatten()
        .map(|intent| {
            crate::live_sessions::LiveSessionTurn::begin(
                turn_context.config.codex_home.as_path(),
                &sess.thread_id.to_string(),
                turn_context.cwd().as_path(),
                intent,
            )
        });
        let mut concurrent_sessions_checked = false;

        // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
        // one instance across retries within this turn.
        // Pending input is drained into history before building the next model request.
        // Startup input already contains older recovered work before the new prompt.
        // Only steering admitted after startup waits for the next sampling safe point;
        // compaction reopens the ordinary pending-input drain.

        let mut next_step_context = Some(first_step_context);
        let mut initial_mailbox_pending = true;
        let mut first_router = Some(first_router);
        // The turn owns its fully rendered instructions; the scaffold clones the
        // text only on a cache miss.
        let base_instructions = Arc::clone(&turn_context.base_instructions);
        'sampling_loop: loop {
            // Metadata belongs to this attempt, while underlying evidence stays in
            // the turn execution owner across repairs and pending-input continuations.
            completion_assessment = None;
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            // Note that pending_input would be something like a message the user
            // submitted through the UI while the model was running. Though the UI
            // may support this, the model might not.
            if logical_generation_budget.is_exhausted() {
                defer_pending_input = true;
                report_logical_generation_budget_exhausted(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &mut generation_budget_error_reported,
                )
                .await;
                break 'sampling_loop;
            }
            // A final-looking item is not terminal authority. Every admitted
            // regular continuation (including provider-false and Stop repairs)
            // reopens delivery before the input snapshot. Exhausted/forced-final
            // turns leave next-turn mail alone and manufacture no drain request.
            if has_started_generation && logical_generation_budget.has_regular_generation_capacity()
            {
                sess.input_queue
                    .accept_mailbox_delivery_for_current_turn(
                        &sess.active_turn,
                        &turn_context.sub_id,
                    )
                    .await;
            }
            let collect_mailbox =
                !can_drain_pending_input && std::mem::take(&mut initial_mailbox_pending);
            let recorded_input = if logical_generation_budget.has_regular_generation_capacity()
                && (can_drain_pending_input || collect_mailbox)
            {
                if can_drain_pending_input {
                    initial_mailbox_pending = false;
                }
                drain_and_record_pending_inputs(&sess, &turn_context, collect_mailbox, &input)
                    .await?
            } else {
                RecordedInputOutcome {
                    should_stop: false,
                    accepted_context_input: false,
                    instructions_changed: false,
                    renews_generation_budget: false,
                }
            };

            if recorded_input.accepted_context_input {
                pending_workspace_prefetch = None;
                prefetched_workspace_identity = None;
                seen_stop_repairs.clear();
                completion_evidence = None;
                provider_overflow_recovery_attempted = false;
                turn_execution.accepted_context_input(recorded_input.instructions_changed);
                output_state = TurnOutputState::default();
            }
            if recorded_input.renews_generation_budget {
                logical_generation_budget.accepted_user_input();
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
            let prebuilt_step_context = next_step_context.is_some();
            let mut step_context = match next_step_context.take() {
                Some(step_context) => step_context,
                None => sess.capture_step_context(Arc::clone(&turn_context)).await?,
            };
            // The prebuilt first step may predate the pending-input drain. Only
            // recorded input, not a later steering arrival, grants new authority.
            Arc::make_mut(&mut step_context).user_intent_epoch =
                sess.input_queue.incorporated_user_intent_epoch();
            if prebuilt_step_context {
                // Accepting that input resets the derived plan, so a sampling fence
                // captured before the drain rejected every turn's first plan update.
                // The model-visible plan is rendered after this point.
                Arc::make_mut(&mut step_context).plan_sampling_revision =
                    Some(sess.services.plan_store.sampling_revision().await);
            }
            let request_baselines = {
                let tracker = turn_diff_tracker.lock().await;
                let mut baselines = turn_execution.baselines_with_tool_exposure_revision(
                    tracker.current_mutation_revision(),
                    turn_context.deferred_tool_activation_revision(),
                );
                baselines.set_attributed_mutation_revision(tracker.attributed_mutation_revision());
                baselines
            };
            let request_signals = if kd4_runtime {
                turn_execution.collector(&request_baselines)
            } else {
                SamplingRequestSignalCollector::default()
            };
            let mut generation_request = pending_generation_request.take().unwrap_or_else(|| {
                if !has_started_generation {
                    turn_execution.initial_generation_request(&request_baselines)
                } else {
                    GenerationRequestDisposition {
                        purpose: match pending_continuation_cause {
                            Some(ContinuationCause::Compaction) => {
                                Some(TurnTimingGenerationPurpose::CompactionRecovery)
                            }
                            Some(ContinuationCause::StopHook) => {
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
            if let Some(repair_revision) = missing_answer_terminal_repair_revision.take()
                && repair_revision != request_baselines.relevant_state_fingerprint()
            {
                // New input/effects reopen ordinary work, rather than carrying a
                // delivery-only fence into a different task state.
                generation_request.terminal_completion_only = false;
                generation_request.purpose = Some(TurnTimingGenerationPurpose::Repair);
            }
            if matches!(
                pending_continuation_cause,
                Some(ContinuationCause::Compaction)
            ) {
                generation_request = rebase_generation_request_after_compaction(
                    generation_request,
                    request_baselines.relevant_state_fingerprint(),
                );
            }
            let previous_rates = sess.state.lock().await.token_info_and_rate_limits().1;
            turn_context.turn_timing_state.observe_credits(
                previous_rates
                    .as_ref()
                    .and_then(|rates| rates.credits.as_ref()),
            );
            let generation_budget_admission =
                logical_generation_budget.admit(generation_request.terminal_completion_only);
            let budget_forced_terminal = match generation_budget_admission {
                LogicalGenerationAdmission::Regular => None,
                LogicalGenerationAdmission::Terminal { forced } => {
                    generation_request = generation_request.require_terminal_completion();
                    forced.then_some(if subagent_context_budget_reached {
                        ForcedTerminalReason::SubagentContextBudget
                    } else {
                        ForcedTerminalReason::GenerationBudget
                    })
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
            if let Some(reason) = budget_forced_terminal {
                record_forced_terminal_budget_boundary(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    reason,
                )
                .await;
            }
            if kd4_runtime {
                record_soft_convergence_directive(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &mut turn_execution,
                    has_started_generation,
                )
                .await?;
            }
            has_started_generation = true;
            if let Some(live_session) = live_session.as_mut() {
                let edited = turn_diff_tracker
                    .lock()
                    .await
                    .changed_paths()
                    .into_iter()
                    .map(|(id, path)| {
                        let resource = turn_context.environments.turn_environments.iter()
                            .find(|environment| environment.environment_id == id)
                            .map_or_else(|| format!("unknown:{id}"), |environment| environment.environment.coordination_resource_id());
                        (resource, path)
                    })
                    .collect();
                // Show other sessions' work before the first edit, then what
                // changed in it after each change to this turn's own edited files.
                if live_session.publish_qualified(edited).await || !concurrent_sessions_checked || live_session.peer_refresh_due() {
                    concurrent_sessions_checked = true;
                    let others = live_session.others().await;
                    let retained = retained_context_notices(
                        sess.clone_history().await.raw_items(),
                        "concurrent_sessions",
                    );
                    if let Some(notice) = live_session.sessions_notice(&others, &retained) {
                        record_context_notice_if_changed(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            "concurrent_sessions",
                            &notice,
                        )
                        .await?;
                    }
                }
            }
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

                world_state = sess
                    .record_step_world_state_if_changed(&world_state, step_context.as_ref())
                    .await?;

                // Request preparation includes notices, routing, and the history snapshot. Pending-turn
                // planning, hooks, input recording, and analytics above remain owned
                // orchestration, but are not part of the model-request preparation lane.
                // The guard is consumed at dispatch, so begin it again for every
                // logical generation rather than measuring only the first request.
                turn_context
                    .turn_timing_state
                    .begin_request_preparation(&mut preparation_timing_guard);

                // Construct the input that we will send to the model.
                let mcp_snapshot = step_context.mcp_tool_snapshot().await;
                if !mcp_snapshot.pending_servers.is_empty() || !mcp_snapshot.failed_servers.is_empty() {
                    record_context_notice_if_changed(
                        sess.as_ref(), turn_context.as_ref(), "mcp_catalog_availability",
                        &format!("MCP discovery is incomplete. Already advertised tools remain usable. Pending servers: {:?}. Failed or cancelled servers: {:?}. A later step may discover more capabilities.", mcp_snapshot.pending_servers, mcp_snapshot.failed_servers),
                    ).await?;
                } else if !mcp_snapshot.temporarily_unavailable {
                    // A coherent capture with nothing pending or failed supersedes an
                    // earlier incomplete-discovery notice; a timed-out capture proves nothing.
                    supersede_context_notice_if_present(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        "mcp_catalog_availability",
                        MCP_DISCOVERY_COMPLETE_NOTICE,
                    )
                    .await?;
                }
                record_sampling_notices(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    step_context
                        .mcp_tool_snapshot()
                        .await
                        .temporarily_unavailable,
                    budget_forced_terminal,
                )
                .await?;
                // Keep this future lazy: the finalized router must record its source
                // notice before we snapshot and normalize history. Otherwise a source
                // change discards this work and captures workspace evidence twice.
                let sampling_request_input = Box::pin(async {
                    let history_snapshot_guard = turn_context
                        .turn_timing_state
                        .begin_local_phase(TurnLocalPhase::HistorySnapshot);
                    let history = sess.clone_history().await;
                    drop(history_snapshot_guard);
                    let normalization_guard = turn_context
                        .turn_timing_state
                        .begin_local_phase(TurnLocalPhase::Normalization);
                    if let Some((baseline, handle)) = pending_workspace_prefetch.take()
                        && continuation_workspace_prefetch_is_current(
                            baseline,
                            turn_diff_tracker.lock().await.current_mutation_revision(),
                            false,
                        )
                    {
                        prefetched_workspace_identity = handle.await.ok();
                    }
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
                                sess.services.git_workspace.as_ref(),
                            )
                            .await
                        }
                    };
                    drop(normalization_guard);
                    prepared
                })
                .instrument(trace_span!("run_turn.prepare_sampling_request_input"))
                // This is read-only preparation, including an owned abort-on-drop
                // workspace prefetch. A blocked capture must not delay interruption
                // until the task owner's forced-abort timeout.
                .or_cancel(&cancellation_token)
                .map_err(|_| CodexErr::TurnAborted);

                let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
                    sess.installation_id.clone(),
                    window_id,
                    CodexResponsesRequestKind::Turn,
                );
                Box::pin(run_sampling_request(
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
                    generation_request.clone(),
                    generation_id.clone(),
                    request_signals.clone(),
                    &mut pending_continuation_cause,
                    cancellation_token.child_token(),
                ))
                .instrument(trace_span!("run_sampling_request",
                    turn_id = %step_context.turn.sub_id,
                    model = %step_context.turn.model_info.slug,
                    cwd = %step_context.turn.cwd().display()))
                .await
            }
            .await;
            // Dispatch normally consumes this guard. A request that fails before
            // dispatch must close it here so error lifecycle and cancellation work
            // remain orchestration rather than request preparation.
            turn_context
                .turn_timing_state
                .finish_request_preparation(&mut preparation_timing_guard);
            let admission_refusal = turn_context
                .extension_data
                .remove::<AdmissionRefusedProjection>();
            match sampling_request_result {
                Ok((sampling_request_output, sampling_request_input)) => {
                    unsampled_overflow_relief = None;
                    let SamplingRequestResult {
                        needs_follow_up: model_needs_follow_up,
                        last_agent_message: sampling_request_last_agent_message,
                        assistant_output,
                        final_message_candidate,
                        settled_state,
                        tool_result_continuation,
                        server_end_turn_false,
                        required_tool_terminal,
                        prefetched_workspace_identity: next_workspace_identity,
                        continuation_workspace_prefetch,
                    } = sampling_request_output;
                    prefetched_workspace_identity = next_workspace_identity;
                    pending_workspace_prefetch = continuation_workspace_prefetch;
                    {
                        let tracker = turn_diff_tracker.lock().await;
                        turn_execution.retain_validation_across_changes(
                            settled_state.mutation_revision,
                            &tracker,
                        );
                    }
                    turn_execution.refresh_plan(
                        if turn_context.collaboration_mode.mode == ModeKind::Plan {
                            None
                        } else {
                            sess.services.plan_store.execution_snapshot().await
                        },
                    );
                    turn_execution.settle(&request_baselines, &request_signals, &settled_state);
                    let progress_kinds = turn_execution.observe_progress(
                        &request_baselines,
                        &request_signals,
                        &settled_state,
                    );
                    turn_execution.capture_acceptance_sources(&step_context,
                        &turn_context.sub_id, settled_state.mutation_revision).await;
                    sess.services.plan_store.publish_acceptance_evidence(
                        turn_execution.acceptance_evidence(
                            &turn_context.sub_id,
                            settled_state.mutation_revision,
                        ),
                    );
                    // All owner outcomes have drained. Settle them even when a
                    // required sibling stops this generation; Drop can retain
                    // only settled failures and live validation questions.
                    if let Some(required_tool_terminal) = required_tool_terminal {
                        if required_tool_terminal.cause != RequiredToolTerminalCause::Blocked {
                            let error = CodexErrorInfo::Other;
                            sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone()).await;
                            sess.send_event(&turn_context, EventMsg::Error(ErrorEvent {
                                message: required_tool_terminal.message.clone(),
                                codex_error_info: Some(error),
                            })).await;
                        }
                        return Ok(TurnTaskResult {
                            stop_cause: None,
                            last_agent_message,
                            accepted_final_message: None,
                            surfaced_result,
                            completion_assessment: None,
                            required_tool_terminal: Some(required_tool_terminal),
                            defer_pending_input: false,
                        });
                    }
                    let readiness = turn_execution.completion_readiness(settled_state.mutation_revision);
                    let (advisory, external) = sess.services.plan_store.completion_advisory(
                        &turn_context.sub_id, settled_state.mutation_revision,
                    ).await;
                    let validation_report_only = finalized_mutation_revision.is_some()
                        || !turn_execution.changed_sources(settled_state.mutation_revision);
                    if let Some(text) = readiness.sampling_notice_for_decision(
                        &mut completion_notice_key, &advisory, &external, validation_report_only,
                    ) {
                        sess.record_conversation_items(&turn_context, &[ResponseItem::Message {
                            id: None, role: "developer".into(),
                            content: vec![ContentItem::InputText { text }],
                            phase: None, internal_chat_message_metadata_passthrough: None,
                        }]).await?;
                    }
                    turn_extension_data.insert(readiness);
                    let terminal_feedback = sess
                        .settle_extension_terminal_intents(&turn_extension_data)
                        .or_cancel(&cancellation_token)
                        .await
                        .map_err(|_| CodexErr::TurnAborted)??;
                    for text in terminal_feedback {
                        sess.record_conversation_items(
                            &turn_context,
                            &[ResponseItem::Message {
                                id: None,
                                role: "developer".into(),
                                content: vec![ContentItem::InputText { text }],
                                phase: None,
                                internal_chat_message_metadata_passthrough: None,
                            }],
                        )
                        .await?;
                    }
                    let output_revision = turn_execution
                        .baselines_with_tool_exposure_revision(
                            settled_state.mutation_revision,
                            settled_state.tool_exposure_revision,
                        )
                        .relevant_state_fingerprint();
                    output_state.observe(
                        assistant_output,
                        final_message_candidate,
                        output_revision,
                        !tool_result_continuation,
                        turn_context.final_output_json_schema.is_none(),
                    );
                    logical_generation_budget.observe_progress(
                        request_signals.observed_successful_process_monitor()
                            || request_signals.observed_yielded_execution(),
                    );
                    if let Some(evidence) = request_signals.completion_evidence_key() {
                        completion_evidence = Some(evidence);
                    }
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
                    // Explicit owner completion and the emergency generation budget
                    // may request one final tool-free response. Repeated actions and
                    // passing validation do not establish completion of the task.
                    let mut needs_follow_up = generation_needs_follow_up(
                        &generation_request,
                        model_needs_follow_up,
                        has_pending_input,
                    );
                    if let Some(reason) = budget_forced_terminal {
                        // The budget grants one final tool-free synthesis request.
                        // Queued input remains in the mailbox for the next turn;
                        // it must not reopen this exhausted sampling loop.
                        defer_pending_input = true;
                        needs_follow_up = false;
                        // A spawned agent's context budget ends with its report as
                        // the result; a turn error would replace it for the parent.
                        if reason == ForcedTerminalReason::GenerationBudget {
                            report_logical_generation_budget_exhausted(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                &mut generation_budget_error_reported,
                            )
                            .await;
                        }
                    }
                    let request_analysis = request_signals.analyze_settled_request();
                    let mut convergence_decision =
                        if kd4_runtime && needs_follow_up && !has_pending_input {
                            Some(turn_execution.evaluate_convergence_with_analysis(
                                &request_baselines,
                                &request_analysis,
                                &settled_state,
                            ))
                        } else {
                            None
                        };
                    let authoritative_wait_terminal_surface =
                        convergence_decision.as_ref().and_then(|decision| {
                            authoritative_wait_terminal_surface(
                                decision,
                                turn_context.final_output_json_schema.as_deref(),
                                // Same ceiling chain as the exec cell budget: a message
                                // that survived cell projection must not bounce to a
                                // model re-emission turn here.
                                turn_context
                                    .config
                                    .tool_output_token_limit
                                    .unwrap_or(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
                                    .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL),
                            )
                        });
                    if authoritative_wait_terminal_surface.is_none()
                        && convergence_decision.as_ref().is_some_and(|decision| {
                            decision.continuation == ContinuationDisposition::SurfaceExistingResult
                        })
                    {
                        // The proposed receipt did not satisfy the final-output
                        // contract. Continue normally; do not record it as delivered.
                        convergence_decision = None;
                    }
                    let terminal_completion_required =
                        convergence_decision.as_ref().is_some_and(|decision| {
                            decision.continuation
                                == ContinuationDisposition::TerminalCompletionRequired
                        });
                    if authoritative_wait_terminal_surface.is_some() {
                        needs_follow_up = false;
                    }
                    let mut next_generation_request = needs_follow_up.then(|| {
                        turn_execution.continuation_generation_request_with_analysis(
                            &request_baselines,
                            &request_analysis,
                            &settled_state,
                            has_pending_input,
                        )
                    });
                    drop(request_analysis);
                    if terminal_completion_required {
                        next_generation_request = next_generation_request
                            .map(GenerationRequestDisposition::require_terminal_completion);
                    }
                    if needs_follow_up
                        && generation_budget_blocks_follow_up(
                            *logical_generation_budget,
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
                        request_signals.structured_action_fingerprint(),
                        request_signals.has_no_structured_actions(),
                        progress_kinds.is_empty()
                            && !request_signals.observed_successful_process_monitor(),
                    );
                    pending_generation_request = next_generation_request;
                    if request_signals.is_wait_only() {
                        turn_context.turn_timing_state.record_wait_only_generation();
                    }
                    // Retire consumed, exactly recoverable evidence above the
                    // steady-state ceiling once re-sending it has cost more than
                    // the cache miss retiring it causes, rather than only at the
                    // compaction limit. This costs no model request. Only a
                    // continuing turn builds a next prompt, so a finishing turn
                    // gains nothing from retiring.
                    let mut token_status = token_status;
                    if needs_follow_up
                        && !token_status.token_limit_reached
                        && retire_consumed_tool_outputs_before_limit(
                            sess.as_ref(),
                            turn_context.as_ref(),
                        )
                        .await
                    {
                        // Later decisions in this iteration compare against the
                        // limit, so they must see the post-retirement usage.
                        token_status = super::context_window::context_window_token_status(
                            sess.as_ref(),
                            turn_context.as_ref(),
                        )
                        .await;
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

                    let defer_compaction = needs_follow_up
                        && token_limit_reached
                        && !final_answer_compaction_deferred
                        && defer_compaction_for_final_answer(
                            turn_execution.plan_completed()
                                || pending_generation_request
                                    .as_ref()
                                    .is_some_and(|request| request.terminal_completion_only)
                                // Admission will force the next request to report the
                                // budget boundary, even without a completed checklist.
                                || (!has_pending_input
                                    && !logical_generation_budget.has_regular_generation_capacity()),
                            token_status.active_context_tokens,
                            turn_context.model_context_window(),
                        );
                    final_answer_compaction_deferred |= defer_compaction;
                    // Automatic compaction verifies that the replacement fits before continuing.
                    if !defer_compaction && needs_follow_up && token_limit_reached {
                        record_convergence_decision(
                            sess.as_ref(),
                            turn_context.as_ref(),
                            &mut turn_execution,
                            convergence_decision.as_mut(),
                        )
                        .await?;
                        reuse_workspace_prefetch_for_local_compaction(
                            &turn_context,
                            &turn_diff_tracker,
                            &mut pending_workspace_prefetch,
                            &mut prefetched_workspace_identity,
                            &cancellation_token,
                        )
                        .await?;
                        let relief = match run_auto_compact(
                            &sess,
                            Arc::clone(&step_context),
                            /*fallback_step_context*/ None,
                            &mut client_session,
                            prefetched_workspace_identity.as_ref(),
                            Some(Arc::clone(&world_state)),
                            CompactionReason::ContextLimit,
                            CompactionPhase::MidTurn,
                            ConsumedOutputRetirement::Eligible {
                                projected_pressure: None,
                            },
                            &cancellation_token,
                        )
                        .await
                        {
                            Ok(relief) => relief,
                            Err(err) => {
                                if matches!(err, CodexErr::TurnAborted) {
                                    return Err(err);
                                }
                                let error = err.to_codex_protocol_error();
                                sess.emit_turn_error_lifecycle(
                                    turn_context.as_ref(),
                                    error.clone(),
                                )
                                .await;
                                // `run_auto_compact` reports no failure itself; this
                                // boundary owns the turn's single terminal error.
                                sess.track_turn_codex_error(turn_context.as_ref(), &err);
                                sess.send_event(
                                    &turn_context,
                                    EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
                                )
                                .await;
                                last_agent_message =
                                    sampling_request_last_agent_message.or(last_agent_message);
                                defer_pending_input |= has_pending_input;
                                break 'sampling_loop;
                            }
                        };
                        can_drain_pending_input = true;
                        pending_continuation_cause = Some(ContinuationCause::Compaction);
                        // Routine retirement is the exact, recoverable operation that
                        // proactive retirement performs uncounted; only pressure it
                        // cannot absorb spends a spawned agent's compaction budget.
                        if relief == ContextPressureRelief::Compaction {
                            subagent_context_budget_reached |= observe_context_pressure_event(
                                &turn_context.session_source,
                                &mut context_pressure_events,
                                logical_generation_budget,
                            );
                        }
                        // A requested reset still commits, but it cannot revoke an
                        // already accepted answer or manufacture another generation.
                        // Completion hooks and pending-input admission remain below.
                        if needs_follow_up {
                            continue;
                        }
                    }

                    if !needs_follow_up {
                        // A delivery candidate has no publication authority until this
                        // completion attempt survives hooks and pending-input admission.
                        let candidate = authoritative_wait_terminal_surface;
                        // Repair delivery before running potentially effectful completion
                        // hooks. Missing prose must not replay an after-agent command.
                        if candidate.is_none()
                            && !output_state.has_answer()
                            && !matches!(turn_context.session_source, SessionSource::Internal(_))
                            && !generation_budget_error_reported
                        {
                            if !output_state.missing_answer_repair_attempted
                                && admit_regular_follow_up(
                                    sess.as_ref(),
                                    turn_context.as_ref(),
                                    logical_generation_budget,
                                    &mut generation_budget_error_reported,
                                )
                                .await
                            {
                                output_state.missing_answer_repair_attempted = true;
                                let repair_revision = turn_diff_tracker.lock().await.current_mutation_revision();
                                let mut readiness = turn_execution.completion_readiness(repair_revision);
                                if finalized_mutation_revision.is_some()
                                    || !turn_execution.changed_sources(repair_revision)
                                {
                                    readiness.validation_work.clear();
                                }
                                let closure = if turn_context.collaboration_mode.mode == ModeKind::Plan {
                                    None
                                } else {
                                    tokio::select! {
                                        _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                                        result = tokio::time::timeout(std::time::Duration::from_secs(5), sess.services.plan_store.task_closure(
                                            &turn_context.sub_id, repair_revision,
                                            sess.services.agent_control.task_coordinator(),
                                        )) => result.ok(),
                                    }
                                };
                                // Satisfied criteria account for their mapped checklist rows
                                // here as on the ordinary final path.
                                if let Some(decision) = &closure {
                                    readiness.reconcile_acceptance(decision);
                                }
                                let delivery_only = closure.as_ref().is_some_and(|decision| {
                                    decision.terminal_action != crate::plan_store::acceptance::TerminalAction::Continue
                                        && readiness.remaining(
                                            decision.request_state == crate::plan_store::acceptance::RequestState::Blocked,
                                            &decision.unmet_obligations,
                                        ).is_empty()
                                }) && turn_diff_tracker.lock().await.current_mutation_revision() == repair_revision;
                                sess.record_conversation_items(
                                    &turn_context,
                                    &[ResponseItem::Message {
                                        id: None,
                                        role: "developer".to_string(),
                                        content: vec![ContentItem::InputText {
                                            text: if delivery_only {
                                                MISSING_ANSWER_REPAIR_DIRECTIVE.to_string()
                                            } else {
                                                "The prior response omitted a final answer, but authoritative work is not established complete. Continue obtainable authorized work; do not repeat completed effects or run unauthorized checks. Deliver the final answer when the existing obligations permit it.".to_string()
                                            },
                                        }],
                                        phase: None,
                                        internal_chat_message_metadata_passthrough: None,
                                    }],
                                )
                                .await?;
                                pending_generation_request = if delivery_only {
                                    let fingerprint = turn_execution.baselines_with_tool_exposure_revision(
                                        repair_revision, settled_state.tool_exposure_revision,
                                    ).relevant_state_fingerprint();
                                    missing_answer_terminal_repair_revision = Some(fingerprint.clone());
                                    Some(GenerationRequestDisposition {
                                        purpose: Some(TurnTimingGenerationPurpose::Repair),
                                        sampling: SamplingGenerationDisposition::DecisionBearing,
                                        relevant_state_fingerprint: fingerprint,
                                        failure_fingerprint: None,
                                        terminal_completion_only: false,
                                    }.require_terminal_completion())
                                } else {
                                    None
                                };
                                pending_continuation_cause = Some(ContinuationCause::StopHook);
                                continue 'sampling_loop;
                            }
                            // A budget refusal was just reported as the terminal
                            // cause; no repair was attempted to report on.
                            if !generation_budget_error_reported {
                                emit_status_affecting_turn_error(
                                    sess.as_ref(),
                                    turn_context.as_ref(),
                                    "The model stopped without delivering a final answer after the bounded answer-repair attempt.",
                                ).await;
                            }
                            return Ok(after_agent_abort_result(None, None, defer_pending_input));
                        }
                        if let Some(authoritative_result) = candidate.as_ref() {
                            last_agent_message = authoritative_result.canonical_message.clone();
                        } else {
                            last_agent_message = output_state.answer_text();
                        }
                        // C1: the direct runtime shares this completion path. It must not skip
                        // the after-agent and completion-stop hooks, because those are the
                        // authoritative, user-configured control over whether a turn may finish.
                        let mut initial_task_decision = None;
                        if turn_context.collaboration_mode.mode != ModeKind::Plan {
                            // Direct delivery and ordinary finals share this boundary.
                            // Streamed content remains provisional; lifecycle Completed
                            // is not a request-acceptance verdict.
                            let closure_revision =
                                turn_diff_tracker.lock().await.current_mutation_revision();
                            sess.services.plan_store.publish_acceptance_evidence(
                                turn_execution.fenced_acceptance_evidence(&step_context,
                                    &turn_context.sub_id, closure_revision).await);
                            let decision = tokio::select! {
                                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                                result = tokio::time::timeout(std::time::Duration::from_secs(5), sess.services.plan_store.task_closure(
                                    &turn_context.sub_id, closure_revision,
                                    sess.services.agent_control.task_coordinator(),
                                )) => match result {
                                    Ok(decision) => decision,
                                    Err(_) => {
                                        emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(), "Task acceptance storage query timed out; final success remains unverified.").await;
                                        return Ok(after_agent_abort_result(last_agent_message, surfaced_result, defer_pending_input));
                                    }
                                },
                            };
                            let decision_text = serde_json::json!(decision).to_string();
                            completion_assessment.get_or_insert_with(Default::default)
                                .task_disposition = Some(decision.disposition());
                            turn_context.extension_data.insert(decision.disposition());
                            if decision.terminal_action
                                == crate::plan_store::acceptance::TerminalAction::Continue
                                && budget_forced_terminal.is_none()
                            {
                                if turn_execution.admit_acceptance_repair(&decision)
                                    && admit_regular_follow_up(
                                        sess.as_ref(),
                                        turn_context.as_ref(),
                                        logical_generation_budget,
                                        &mut generation_budget_error_reported,
                                    )
                                    .await
                                {
                                    sess.record_conversation_items(&turn_context, &[ResponseItem::Message {
                                        id: None, role: "developer".into(),
                                        content: vec![ContentItem::InputText { text: format!(
                                            "Declared required acceptance remains open: {decision_text}. Continue independent obtainable authorized work; do not run tests without permission. A partial report does not discharge these obligations. Record concrete external blockers only where work cannot proceed.") }],
                                        phase: None, internal_chat_message_metadata_passthrough: None,
                                    }]).await?;
                                    output_state.invalidate_for_repair();
                                    pending_generation_request = None;
                                    pending_continuation_cause = Some(ContinuationCause::StopHook);
                                    continue 'sampling_loop;
                                }
                                emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(),
                                    "Required declared acceptance remains open after the bounded repair; final success was not accepted.").await;
                                return Ok(after_agent_abort_result(
                                    last_agent_message,
                                    surfaced_result,
                                    defer_pending_input,
                                ));
                            }
                            if turn_diff_tracker.lock().await.current_mutation_revision()
                                != closure_revision
                            {
                                emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(),
                                    "Workspace changed during task acceptance; final success was not accepted.").await;
                                return Ok(after_agent_abort_result(
                                    last_agent_message,
                                    surfaced_result,
                                    defer_pending_input,
                                ));
                            }
                            initial_task_decision = Some(decision);
                        }
                        // Collect writer overlap as an advisory, never causal
                        // proof that this turn's consumer failure is external.
                        if turn_execution.has_external_validation_candidates()
                            && !matches!(
                                turn_context.session_source,
                                SessionSource::SubAgent(_) | SessionSource::Internal(_)
                            )
                        {
                            let thread_id = sess.thread_id.to_string();
                            let others = tokio::select! {
                                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                                others = crate::live_sessions::others_in_checkout(
                                    turn_context.config.codex_home.as_path(),
                                    &thread_id,
                                    turn_context.cwd().as_path(),
                                ) => others,
                            };
                            turn_execution
                                .observe_concurrent_writers(&others, turn_context.cwd().as_path());
                        }
                        let mut readiness =
                            turn_execution.completion_readiness(settled_state.mutation_revision);
                        if let Some(decision) = &initial_task_decision {
                            readiness.reconcile_acceptance(decision);
                        }
                        if finalized_mutation_revision.is_some()
                            || !turn_execution.changed_sources(settled_state.mutation_revision)
                        {
                            // Validation work can only be reported here. After
                            // the finalizer, its directive forbids edits that
                            // would need another finalization. A turn that
                            // changed no sources observed its failures against
                            // the turn-start sources; its final reports them.
                            // Plan and live validation work, declared acceptance
                            // and goal completion still apply. The assessment
                            // keeps failed checks and verification gaps.
                            readiness.validation_work.clear();
                        }
                        let blocked = initial_task_decision.as_ref().is_some_and(|decision| {
                            decision.request_state
                                == crate::plan_store::acceptance::RequestState::Blocked
                        });
                        let remaining = readiness.remaining(
                            blocked,
                            initial_task_decision
                                .as_ref()
                                .map(|decision| decision.unmet_obligations.as_slice())
                                .unwrap_or_default(),
                        );
                        if !remaining.is_empty() && budget_forced_terminal.is_none() {
                            if turn_execution.admit_completion_repair(&readiness)
                                && admit_regular_follow_up(
                                    sess.as_ref(),
                                    turn_context.as_ref(),
                                    logical_generation_budget,
                                    &mut generation_budget_error_reported,
                                )
                                .await
                            {
                                sess.record_conversation_items(&turn_context, &[ResponseItem::Message {
                                    id: None, role: "developer".into(), content: vec![ContentItem::InputText {
                                        text: format!("Known current work prevents final acceptance: {}. Continue only obtainable work the task already authorizes; this notice adds no authorization. When test execution is authorized, resolve a failed check by rerunning it, after any needed fix, through its runner or test command itself, not a wrapper script; otherwise record why it is blocked. Do not launch a duplicate of validation that is still running, or treat a stored process number as a capability. Reconcile legitimate blockers and user-approved scope changes with update_plan. No tool reports validation status beyond the evidence quoted here.", remaining.join(" ")),
                                    }], phase: None, internal_chat_message_metadata_passthrough: None,
                                }]).await?;
                                output_state.invalidate_for_repair();
                                pending_generation_request = None;
                                pending_continuation_cause = Some(ContinuationCause::StopHook);
                                continue 'sampling_loop;
                            }
                            emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(),
                                "Known current work remains unresolved after bounded reconciliation; task success was not accepted.").await;
                            return Ok(after_agent_abort_result(
                                last_agent_message,
                                surfaced_result,
                                defer_pending_input,
                            ));
                        }
                        let mut finalizer_ran = false;
                        let mutating_finalizer_aborted = if matches!(
                            turn_context.config.after_agent_policy,
                            AfterAgentPolicy::MutatingFinalizer
                        ) && turn_context
                            .config
                            .notify
                            .as_ref()
                            .is_some_and(|command| !command.is_empty())
                        {
                            // An uncertain check that left the Git-visible tree
                            // (outside Git, the bounded workspace tree)
                            // unchanged did not change the finalized state.
                            if finalized_mutation_revision.is_some_and(|revision| {
                                turn_execution.validation_revision_at(revision)
                                    != turn_execution
                                        .validation_revision_at(settled_state.mutation_revision)
                            }) {
                                emit_status_affecting_turn_error(
                                    sess.as_ref(), turn_context.as_ref(),
                                    "The workspace changed after the mutating finalizer ran. Completion requires finalization of the repaired state; the finalizer was not replayed because it has no repeat-safety contract.",
                                ).await;
                                return Ok(after_agent_abort_result(
                                    last_agent_message,
                                    surfaced_result,
                                    defer_pending_input,
                                ));
                            }
                            if finalized_mutation_revision.is_none() {
                                finalizer_ran = true;
                                let after_agent_outcome = run_legacy_after_agent_hook(
                                    &sess,
                                    &turn_context,
                                    &sampling_request_input,
                                    last_agent_message.clone(),
                                )
                                .await;
                                // Finalizers can partially mutate even when they fail.
                                // Retire the shared pre-hook baseline before another
                                // command can reuse it as its pre-execution snapshot.
                                sess.services.git_workspace.note_host_workspace_mutation();
                                let mut tracker = turn_diff_tracker.lock().await;
                                tracker.record_unknown_mutation();
                                finalized_mutation_revision =
                                    Some(tracker.current_mutation_revision());
                                pending_workspace_prefetch = None;
                                prefetched_workspace_identity = None;
                                after_agent_outcome.aborted
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if mutating_finalizer_aborted {
                            return Ok(after_agent_abort_result(
                                last_agent_message,
                                surfaced_result,
                                defer_pending_input,
                            ));
                        }
                        if finalizer_ran {
                            // Finalization invalidates the candidate's validation boundary.
                            // Run the unsafe hook once, then let the existing validation
                            // workflow verify the resulting state or report its limitation.
                            if admit_regular_follow_up(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                logical_generation_budget,
                                &mut generation_budget_error_reported,
                            )
                            .await
                            {
                                sess.record_conversation_items(&turn_context, &[ResponseItem::Message {
                                    id: None,
                                    role: "developer".to_string(),
                                    content: vec![ContentItem::InputText { text:
                                        "The mutating finalizer has run exactly once. Earlier validation does not cover the final workspace. Validate the affected final state before accepting an answer, or truthfully report unavailable verification. Do not replay the finalizer; further workspace edits cannot be finalized safely in this turn.".to_string() }],
                                    phase: None,
                                    internal_chat_message_metadata_passthrough: None,
                                }]).await?;
                                pending_generation_request = None;
                                pending_continuation_cause = Some(ContinuationCause::StopHook);
                                output_state.invalidate_for_repair();
                                continue 'sampling_loop;
                            }
                            return Ok(after_agent_abort_result(
                                last_agent_message,
                                surfaced_result,
                                defer_pending_input,
                            ));
                        }
                        let completion_stop_report = run_completion_stop_hook(
                            &sess,
                            &turn_context,
                            stop_hook_active,
                            last_agent_message.clone(),
                            || async {
                                let (
                                    changed_paths,
                                    has_untracked_changes,
                                    unattributed_paths,
                                    changed_existing,
                                    uncertain_contents,
                                    made_attributed_edits,
                                ) = {
                                    let tracker = turn_diff_tracker.lock().await;
                                    (
                                        Some(tracker.changed_paths()),
                                        tracker.has_untracked_changes(),
                                        tracker.unattributed_display_paths(),
                                        tracker.changed_existing_contents(),
                                        tracker.uncertain_content_paths(),
                                        tracker.attributed_mutation_revision() > 0,
                                    )
                                };
                                // Parsing is CPU work bounded by its own deadline.
                                let analysis_paths = changed_existing
                                    .iter()
                                    .map(|(path, ..)| path.clone())
                                    .collect();
                                let mut changed_tests = tokio::task::spawn_blocking(move || {
                                    changed_existing_tests(changed_existing)
                                })
                                .await
                                .unwrap_or_else(|_| ExistingTestChanges {
                                    changed: Vec::new(),
                                    incomplete: analysis_paths,
                                });
                                changed_tests.incomplete.extend(uncertain_contents);
                                changed_tests.incomplete.sort();
                                changed_tests.incomplete.dedup();
                                let runtime_inputs = step_context
                                    .loaded_agents_md
                                    .as_ref()
                                    .map(|loaded| loaded.runtime_input_paths())
                                    .unwrap_or_default();
                                let mut assessment = turn_execution.completion_assessment_with_runtime_inputs(
                                    settled_state.mutation_revision,
                                    changed_paths.as_deref(),
                                    has_untracked_changes,
                                    &runtime_inputs,
                                );
                                if let Some(advisory) = unattributed_changes_advisory(&unattributed_paths, made_attributed_edits) {
                                    assessment.advisories.push(advisory);
                                }
                                if let Some(advisory) = changed_tests_advisory(&changed_tests.changed) {
                                    assessment.advisories.push(advisory);
                                }
                                if let Some(advisory) = incomplete_test_evidence_advisory(&changed_tests.incomplete) {
                                    assessment.advisories.push(advisory);
                                }
                                if finalized_mutation_revision.is_some_and(|revision| {
                                    turn_execution.validation_revision_at(revision)
                                        != turn_execution
                                            .validation_revision_at(settled_state.mutation_revision)
                                }) {
                                    assessment.verification_gaps.push("A mutating finalizer ran after the last settled validation state. That validation does not cover the final workspace; the hook was not replayed because it has no repeat-safety contract.".to_string());
                                }
                                assessment
                            },
                            &turn_diff_tracker,
                            &cancellation_token,
                        )
                        .await;
                        completion_assessment = Some(completion_stop_report.assessment.clone());
                        if let Some(hook_prompt_message) =
                            completion_stop_report.continuation_prompt
                        {
                            let mut repair_state = settled_state.clone();
                            repair_state.mutation_revision =
                                turn_diff_tracker.lock().await.current_mutation_revision();
                            let repair = (
                                completion_stop_report
                                    .continuation_feedback
                                    .iter()
                                    .map(|feedback| {
                                        crate::tools::context::normalize_observation_text(feedback)
                                    })
                                    .collect::<Vec<_>>(),
                                turn_execution.settled_revision_key(&repair_state),
                                completion_evidence.clone(),
                                last_agent_message.clone(),
                            );
                            if seen_stop_repairs.contains(&repair) {
                                emit_status_affecting_turn_error(
                                    sess.as_ref(), turn_context.as_ref(),
                                    "Stop hook repeated the same blocking feedback without changed state or evidence after a repair attempt.",
                                ).await;
                                return Ok(after_agent_abort_result(
                                    last_agent_message,
                                    surfaced_result,
                                    defer_pending_input,
                                ));
                            }
                            seen_stop_repairs.push(repair);
                            if admit_regular_follow_up(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                logical_generation_budget,
                                &mut generation_budget_error_reported,
                            )
                            .await
                            {
                                sess.record_response_item_and_emit_turn_item(
                                    &turn_context,
                                    hook_prompt_message,
                                )
                                .await?;
                                stop_hook_active = true;
                                pending_generation_request = None;
                                pending_continuation_cause = Some(ContinuationCause::StopHook);
                                output_state.invalidate_for_repair();
                                continue 'sampling_loop;
                            }
                        }
                        if completion_stop_report.should_block {
                            emit_status_affecting_turn_error(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                "Stop hook blocked completion; no actionable repair could be admitted.",
                            )
                            .await;
                            return Ok(after_agent_abort_result(
                                last_agent_message,
                                surfaced_result,
                                defer_pending_input,
                            ));
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
                            *logical_generation_budget,
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
                        let completion_mutation_revision =
                            turn_diff_tracker.lock().await.current_mutation_revision();
                        // An approved Stop command always records one unknown
                        // mutation, even when it only reports a verdict. Its
                        // receipt may preserve this candidate, never validation
                        // evidence or another writer's/context revision.
                        let answer_mutation_revision = if completion_stop_report
                            .mutation_receipt
                            .as_ref()
                            .is_some_and(|receipt| {
                                receipt.covers_only_hook_effect(
                                    settled_state.mutation_revision,
                                    completion_mutation_revision,
                                )
                            }) {
                            settled_state.mutation_revision
                        } else {
                            completion_mutation_revision
                        };
                        let completion_output_revision = turn_execution
                            .baselines_with_tool_exposure_revision(
                                answer_mutation_revision,
                                turn_context.deferred_tool_activation_revision(),
                            )
                            .relevant_state_fingerprint();
                        output_state.reconcile_revision(&completion_output_revision);
                        if candidate.is_none()
                            && !output_state.has_answer()
                            && !matches!(turn_context.session_source, SessionSource::Internal(_))
                            && !generation_budget_error_reported
                        {
                            emit_status_affecting_turn_error(
                                sess.as_ref(),
                                turn_context.as_ref(),
                                "Completion hooks changed the answer's task state. A current final answer is unavailable; effectful hooks were not replayed.",
                            ).await;
                            return Ok(after_agent_abort_result(None, None, defer_pending_input));
                        }
                        if let Some(mut initial_decision) = initial_task_decision.clone() {
                            let revision =
                                turn_diff_tracker.lock().await.current_mutation_revision();
                            sess.services.plan_store.publish_acceptance_evidence(
                                turn_execution.fenced_acceptance_evidence(&step_context,
                                    &turn_context.sub_id, revision).await);
                            let decision = tokio::select! {
                                _ = cancellation_token.cancelled() => return Err(CodexErr::TurnAborted),
                                result = tokio::time::timeout(std::time::Duration::from_secs(5),
                                    sess.services.plan_store.task_closure(&turn_context.sub_id, revision,
                                        sess.services.agent_control.task_coordinator())) => match result {
                                    Ok(decision) => decision,
                                    Err(_) => {
                                        emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(), "Task acceptance storage query timed out; final success remains unverified.").await;
                                        return Ok(after_agent_abort_result(last_agent_message, surfaced_result, defer_pending_input));
                                    }
                                },
                            };
                            completion_assessment.get_or_insert_with(Default::default)
                                .task_disposition = Some(decision.disposition());
                            turn_context.extension_data.insert(decision.disposition());
                            // Only a declared contract that remains open is a host
                            // observation. A no-contract (unknown) or satisfied
                            // disposition is not an advisory for the turn or its parent.
                            if !decision.unmet_obligations.is_empty() {
                                completion_assessment
                                    .get_or_insert_with(Default::default)
                                    .advisories
                                    .push(format!(
                                        "Task disposition: {}",
                                        serde_json::json!(decision)
                                    ));
                            }
                            if completion_stop_report
                                .mutation_receipt
                                .as_ref()
                                .is_some_and(|receipt| {
                                    receipt.covers_only_hook_effect(
                                        initial_decision.mutation_revision,
                                        revision,
                                    )
                                })
                            {
                                // Compare actual post-hook evidence and obligations.
                                // Only the receipted counter difference is expected;
                                // stale validation must still change the decision.
                                initial_decision.mutation_revision = revision;
                            }
                            if decision != initial_decision
                                || turn_diff_tracker.lock().await.current_mutation_revision()
                                    != revision
                            {
                                emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(),
                                    "Task, evidence or required child changed during completion hooks; final success was not accepted and effectful hooks were not replayed.").await;
                                return Ok(after_agent_abort_result(
                                    last_agent_message,
                                    surfaced_result,
                                    defer_pending_input,
                                ));
                            }
                            initial_task_decision = Some(decision);
                        }
                        if let Some(decision) = &initial_task_decision {
                            let owner = Arc::clone(&sess);
                            let persistence_owner = Arc::clone(&sess);
                            let decision = decision.clone();
                            let result = sess.terminal_tasks.spawn(async move {
                                owner.services.plan_store.archive_completed_scope(&decision, |item| async move {
                                    persistence_owner.persist_rollout_items_durable(&[
                                        codex_protocol::protocol::RolloutItem::ResponseItem(item),
                                    ]).await
                                }).await
                            }).await;
                            match result.unwrap_or_else(|error| Err(std::io::Error::other(error.to_string()))) {
                                // Archival can complete lagging mapped rows; publish the
                                // durable checklist after persistence, as update_plan does.
                                Ok(Some(plan)) => {
                                    sess.send_event(turn_context.as_ref(), EventMsg::PlanUpdate(plan)).await;
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    emit_status_affecting_turn_error(sess.as_ref(), turn_context.as_ref(),
                                        &format!("Completed task scope could not be archived: {error}; final acceptance remains uncommitted.")).await;
                                    return Ok(after_agent_abort_result(last_agent_message, surfaced_result, defer_pending_input));
                                }
                            }
                        }
                        surfaced_result = candidate;
                        if surfaced_result.is_some() {
                            turn_context.turn_timing_state.record_surfaced_final_answer();
                        }
                        accepted_final_message = if surfaced_result.is_none() {
                            last_agent_message = output_state.answer_text();
                            output_state.take_answer()
                        } else {
                            None
                        };
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
                        &mut turn_execution,
                        convergence_decision.as_mut(),
                    )
                    .await?;
                    debug_assert!(pending_continuation_cause.is_some());
                    continue;
                }
                Err(err @ CodexErr::TurnAborted) => {
                    return Err(err);
                }
                Err(CodexErr::ContextWindowExceeded)
                    if unsampled_overflow_relief != Some(ContextPressureRelief::Compaction)
                        && (admission_refusal.is_some()
                            || !provider_overflow_recovery_attempted) =>
                {
                    let retire_consumed_outputs = match admission_refusal.as_deref() {
                        // Admission refused this projection before dispatch. Like a
                        // limit found after sampling, it is relieved for each crossing,
                        // first by exact retirement judged against that projection.
                        Some(AdmissionRefusedProjection(projected_pressure))
                            if unsampled_overflow_relief.is_none() =>
                        {
                            ConsumedOutputRetirement::Eligible {
                                projected_pressure: Some(*projected_pressure),
                            }
                        }
                        // Retirement did not make room for this prompt.
                        Some(_) => ConsumedOutputRetirement::Unavailable,
                        // The provider rejected the prompt, so the local estimate
                        // already failed; only a real summary can be trusted here.
                        None => {
                            provider_overflow_recovery_attempted = true;
                            ConsumedOutputRetirement::Unavailable
                        }
                    };
                    if !admit_regular_follow_up(
                        sess.as_ref(),
                        turn_context.as_ref(),
                        logical_generation_budget,
                        &mut generation_budget_error_reported,
                    )
                    .await
                    {
                        break;
                    }
                    reuse_workspace_prefetch_for_local_compaction(
                        &turn_context,
                        &turn_diff_tracker,
                        &mut pending_workspace_prefetch,
                        &mut prefetched_workspace_identity,
                        &cancellation_token,
                    )
                    .await?;
                    let relief = match run_auto_compact(
                        &sess,
                        Arc::clone(&step_context),
                        None,
                        &mut client_session,
                        prefetched_workspace_identity.as_ref(),
                        Some(Arc::clone(&world_state)),
                        CompactionReason::ContextLimit,
                        CompactionPhase::MidTurn,
                        retire_consumed_outputs,
                        &cancellation_token,
                    )
                    .await
                    {
                        Ok(relief) => relief,
                        Err(err) => {
                            if matches!(err, CodexErr::TurnAborted) {
                                return Err(err);
                            }
                            sess.emit_turn_error_lifecycle(
                                turn_context.as_ref(),
                                err.to_codex_protocol_error(),
                            )
                            .await;
                            sess.track_turn_codex_error(turn_context.as_ref(), &err);
                            sess.send_event(
                                &turn_context,
                                EventMsg::Error(err.to_error_event(None)),
                            )
                            .await;
                            break;
                        }
                    };
                    unsampled_overflow_relief = Some(relief);
                    pending_generation_request = None;
                    pending_continuation_cause = Some(ContinuationCause::Compaction);
                    if relief == ContextPressureRelief::Compaction {
                        subagent_context_budget_reached |= observe_context_pressure_event(
                            &turn_context.session_source,
                            &mut context_pressure_events,
                            logical_generation_budget,
                        );
                    }
                    continue;
                }
                Err(codex_error @ CodexErr::InvalidImageRequest()) => {
                    // This provider error has no item/index attribution. Do not
                    // destroy valid siblings or make a retry/resume see different
                    // canonical evidence by guessing which image was rejected.
                    sess.track_turn_codex_error(turn_context.as_ref(), &codex_error);
                    let error = CodexErrorInfo::BadRequest;
                    sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                        .await;
                    let event = EventMsg::Error(ErrorEvent {
                        message: "The provider rejected an image but did not identify which one. No images were removed. Please review the attached images before retrying."
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
            stop_cause: subagent_context_budget_reached
                .then_some(codex_protocol::protocol::TurnExecutionStopCause::SubagentContextBudget),
            last_agent_message,
            accepted_final_message,
            surfaced_result,
            completion_assessment,
            required_tool_terminal: None,
            defer_pending_input,
        })
    })
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
    _surfaced_result: Option<SurfacedToolResult>,
    defer_pending_input: bool,
) -> TurnTaskResult {
    TurnTaskResult {
        stop_cause: None,
        last_agent_message,
        accepted_final_message: None,
        // Rejected output remains in tool/history evidence, not accepted output.
        surfaced_result: None,
        completion_assessment: None,
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
    record_inspected_inputs(sess, turn_context, input).await?;
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
    // Planning never drained the queue, so input that is still pending would
    // re-enter this task and repeat the same failure without bound.
    Ok(TurnTaskResult {
        defer_pending_input: true,
        ..TurnTaskResult::default()
    })
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
    schema: Option<&super::turn_context::PreparedOutputSchema>,
    token_limit: usize,
) -> Option<SurfacedToolResult> {
    if decision.continuation != ContinuationDisposition::SurfaceExistingResult {
        return None;
    }
    match &decision.authoritative_wait {
        Some(AuthoritativeWaitResolution::Terminal(result)) => {
            if let Some(message) = result.surfaceable_message.as_deref() {
                if message.trim().is_empty()
                    || codex_utils_output_truncation::model_token_count(message) > token_limit
                {
                    return None;
                }
                if let Some(schema) = schema
                    && schema.validate_text(Some(message)).is_err()
                {
                    return None;
                }
            } else if schema.is_some() {
                return None;
            }
            Some(SurfacedToolResult {
                adapter: result.adapter.clone(),
                value: result.value.clone(),
                canonical_message: result.surfaceable_message.clone(),
            })
        }
        Some(AuthoritativeWaitResolution::Blocked(_)) | None => None,
    }
}

// Explicit emergency budget, not a claim about remaining reasoning scope.
// Retained-evidence analysis can use the entire allowance. Identical action/result
// cycles are suppressed by the existing deterministic-cycle machinery instead
// of treating evidence novelty as reasoning progress.
const MAX_REGULAR_LOGICAL_GENERATIONS: u32 = 500;
// Owned monitoring is free up to this many generations per renewal. Uncapped
// credits let a turn that keeps polling or yielding run past the budget
// without ever reaching its forced synthesis.
const MAX_MONITORING_GENERATION_CREDITS: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalGenerationAdmission {
    Regular,
    Terminal { forced: bool },
    Exhausted,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct LogicalGenerationBudget {
    regular_generations: u32,
    terminal_generation_used: bool,
    monitoring_credits: u32,
}

impl LogicalGenerationBudget {
    fn observe_progress(&mut self, successful_process_monitor: bool) {
        if successful_process_monitor
            && self.regular_generations > 0
            && self.monitoring_credits < MAX_MONITORING_GENERATION_CREDITS
        {
            self.regular_generations -= 1;
            self.monitoring_credits += 1;
        }
    }

    fn accepted_user_input(&mut self) {
        self.regular_generations = 0;
        self.terminal_generation_used = false;
        self.monitoring_credits = 0;
    }

    /// The next regular request becomes the one forced final synthesis.
    fn exhaust_regular_generations(&mut self) {
        self.regular_generations = MAX_REGULAR_LOGICAL_GENERATIONS;
    }

    fn is_exhausted(self) -> bool {
        self.regular_generations >= MAX_REGULAR_LOGICAL_GENERATIONS && self.terminal_generation_used
    }

    fn has_regular_generation_capacity(self) -> bool {
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

    fn can_admit(self, terminal_requested: bool) -> bool {
        let mut preview = self;
        preview.admit(terminal_requested) != LogicalGenerationAdmission::Exhausted
    }
}

fn generation_budget_blocks_follow_up(
    budget: LogicalGenerationBudget,
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
    budget: LogicalGenerationBudget,
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

const LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE: &str = "The turn exhausted its explicit generation safety budget. This does not establish exhaustion of useful reasoning or evidence. Work is suspended, not completed. This is the final tool-free synthesis request. Do not call tools. Summarize completed work and truthfully report remaining work, failed validation, running processes, and how to resume.";

/// A spawned agent that fills this many context windows in one turn reports
/// what it covered instead of compacting again; its parent splits the rest.
const MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS: u32 = 2;
const SUBAGENT_CONTEXT_BUDGET_FORCED_TERMINAL_DIRECTIVE: &str = "This spawned agent reached its context budget. Work is suspended, not completed. This is the final tool-free synthesis request. Do not call tools. Report what you covered, and list the unread files or byte ranges and any unfinished work exactly, so the parent can split the remaining scope.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForcedTerminalReason {
    GenerationBudget,
    SubagentContextBudget,
}

/// Counts a mid-turn context-pressure event. A spawned agent at its budget
/// gets one forced final synthesis instead of another context window.
fn observe_context_pressure_event(
    session_source: &SessionSource,
    events: &mut u32,
    budget: &mut LogicalGenerationBudget,
) -> bool {
    *events = events.saturating_add(1);
    let reached = *events >= MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS
        && matches!(
            session_source,
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. })
        );
    if reached {
        budget.exhaust_regular_generations();
    }
    reached
}

async fn record_forced_terminal_budget_boundary(
    sess: &Session,
    turn_context: &TurnContext,
    reason: ForcedTerminalReason,
) {
    let message = match reason {
        ForcedTerminalReason::GenerationBudget => format!(
            "This turn reached its explicit safety budget of {MAX_REGULAR_LOGICAL_GENERATIONS} generations. Work is suspended; the assistant will report completed and unfinished work. Send another message to resume."
        ),
        ForcedTerminalReason::SubagentContextBudget => format!(
            "This spawned agent reached its budget of {MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS} compactions in one turn. It will report what it covered and the unread scope so its parent can split the remaining work."
        ),
    };
    sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
        .await;
}

/// The first user-authored text of a turn, which names what it is doing.
fn turn_input_first_text(input: &[TurnInput]) -> Option<&str> {
    input.iter().find_map(|item| match item {
        TurnInput::UserInput { content, .. } => content.iter().find_map(|content| match content {
            UserInput::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        }),
        _ => None,
    })
}

/// Names observed changes the turn did not make, without counting them as its own.
/// A turn without attributed edits is told so: its commands remain a possible
/// cause, but the useful action is re-reading evidence gathered before the change.
fn unattributed_changes_advisory(paths: &[String], made_attributed_edits: bool) -> Option<String> {
    (!paths.is_empty()).then(|| {
        let paths = crate::session::turn_execution::bounded_path_list(paths);
        if made_attributed_edits {
            format!(
                "Workspace changes observed during this turn were not attributed to it (another writer, or an undeclared command effect): {paths}. Re-read these paths before relying on evidence gathered earlier."
            )
        } else {
            format!(
                "This turn made no attributed edits, but these paths changed during it (another writer, or an undeclared effect of a command it ran): {paths}. Evidence read from them earlier may be stale; re-read before relying on it."
            )
        }
    })
}

/// Test files by common convention: a test directory or a test-named file.
/// This only selects paths for the advisory below; it discovers no tests.
fn conventional_test_path(path: &str) -> bool {
    const TEST_DIRECTORIES: [&str; 5] = ["test", "tests", "__tests__", "spec", "specs"];
    const TEST_SUFFIXES: [&str; 6] = [
        "_test.py",
        "_tests.py",
        "_test.go",
        "_test.rs",
        "_tests.rs",
        "_spec.rb",
    ];
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let mut components = normalized.split('/').collect::<Vec<_>>();
    let Some(name) = components.pop() else {
        return false;
    };
    components
        .iter()
        .any(|component| TEST_DIRECTORIES.contains(component) || component.ends_with(".tests"))
        || name.starts_with("test_")
        || matches!(name, "tests.rs" | "conftest.py")
        || TEST_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// Display paths of existing files whose existing tests this turn edited or
/// removed: test items in Rust and Python outlines, including inline
/// `#[cfg(test)]` modules, or conventional test files. Adding a test changes
/// no existing assertion and is not reported.
#[derive(Default)]
struct ExistingTestChanges {
    changed: Vec<String>,
    incomplete: Vec<String>,
}

fn changed_existing_tests(files: Vec<(String, String, Option<String>)>) -> ExistingTestChanges {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let mut result = ExistingTestChanges::default();
    for (path, before, after) in files {
        let conventional = conventional_test_path(&path);
        match crate::tools::handlers::existing_tests_changed(
            &path,
            &before,
            after.as_deref(),
            conventional,
            deadline,
        ) {
            Some(true) => result.changed.push(path),
            Some(false) => {}
            None => {
                // A failed/unsupported outline is not evidence of unchanged
                // tests. Keep the conventional-file fallback and expose its
                // uncertainty separately, without blaming an unproven edit.
                if conventional && rewrites_original_lines(&before, after.as_deref()) {
                    result.changed.push(path.clone());
                }
                if conventional || path.ends_with(".rs") || path.ends_with(".py") {
                    result.incomplete.push(path);
                }
            }
        }
    }
    result
}

fn rewrites_original_lines(before: &str, after: Option<&str>) -> bool {
    let Some(after) = after else {
        return true;
    };
    let mut config = similar::TextDiff::configure();
    config.timeout(std::time::Duration::from_millis(100));
    config.diff_lines(before, after).ops().iter().any(|op| {
        matches!(
            op.tag(),
            similar::DiffTag::Delete | similar::DiffTag::Replace
        )
    })
}

/// An edited or removed existing test can change what the suite asserts, and
/// a final answer need not mention it, so completion names those files.
fn changed_tests_advisory(paths: &[String]) -> Option<String> {
    (!paths.is_empty()).then(|| {
        format!(
            "This turn modified or deleted existing test files; review any changed expectations or execution policy: {}.",
            crate::session::turn_execution::bounded_path_list(paths)
        )
    })
}

fn incomplete_test_evidence_advisory(paths: &[String]) -> Option<String> {
    (!paths.is_empty()).then(|| format!(
        "Existing-test evidence could not be fully assessed (incomplete analysis or uncertain before/after attribution): {}. Do not treat this as proof that tests were unchanged or attribute uncertain edits to this turn.",
        crate::session::turn_execution::bounded_path_list(paths)
    ))
}

async fn record_context_notice_if_changed(
    sess: &Session,
    turn: &TurnContext,
    marker: &str,
    text: &str,
) -> CodexResult<bool> {
    let opening = format!("<{marker}>");
    let rendered = format!("{opening}\n{text}\n</{marker}>");
    let history = sess.clone_history().await;
    let previous = history
        .raw_items()
        .iter()
        .rev()
        .find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().find_map(|content| match content {
                    ContentItem::InputText { text } if text.starts_with(&opening) => Some(text),
                    _ => None,
                })
            }
            _ => None,
        });
    if previous == Some(&rendered) || (previous.is_none() && text.is_empty()) {
        return Ok(false);
    }
    let item = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text: rendered }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    sess.record_conversation_items(turn, &[item]).await?;
    Ok(true)
}

/// Settled state that supersedes an earlier incomplete-discovery notice.
const MCP_DISCOVERY_COMPLETE_NOTICE: &str = "MCP discovery is complete: no servers are pending, failed, or cancelled. Earlier incomplete-discovery notices no longer apply.";

/// Supersede a delivered `<marker>` notice with `text`. Records nothing when
/// history holds no such notice, so a session never told otherwise sees nothing.
async fn supersede_context_notice_if_present(
    sess: &Session,
    turn: &TurnContext,
    marker: &str,
    text: &str,
) -> CodexResult<bool> {
    let opening = format!("<{marker}>");
    let delivered = sess.clone_history().await.raw_items().iter().any(|item| {
        matches!(item, ResponseItem::Message { role, content, .. }
            if role == "developer"
                && content.iter().any(|content| matches!(content,
                    ContentItem::InputText { text: body } if body.starts_with(&opening))))
    });
    if !delivered {
        return Ok(false);
    }
    record_context_notice_if_changed(sess, turn, marker, text).await
}

/// Bodies of the `<marker>` notices in history, oldest first, as
/// `record_context_notice_if_changed` renders them.
fn retained_context_notices(items: &[ResponseItem], marker: &str) -> Vec<String> {
    let (opening, closing) = (format!("<{marker}>\n"), format!("\n</{marker}>"));
    items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            ContentItem::InputText { text } => text
                .strip_prefix(&opening)?
                .strip_suffix(&closing)
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Rough text capacity illustration, not a measured token or timing model.
const SCOPE_ESTIMATE_BYTES_PER_TOKEN: i64 = 4;

/// Spawned agents always need their host-enforced budget, regardless of wording.
fn spawned_agent_context_budget_notice(turn: &TurnContext) -> String {
    let mut notice = String::from(
        "Before committing to an exhaustive review or full-read request, use the known file count, byte volume, and tool throughput to assess whether the requested coverage is feasible.",
    );
    if let Some(limit) = turn
        .model_info
        .auto_compact_token_limit()
        .or_else(|| turn.model_context_window())
        .filter(|limit| *limit > 0)
    {
        // Illustrate a reserve for instructions, history, and output. This is
        // not current headroom or a prediction of the number of compactions.
        let inline_bytes = limit.saturating_mul(3) / 4 * SCOPE_ESTIMATE_BYTES_PER_TOKEN;
        let megabytes = inline_bytes as f64 / 1_000_000.0;
        notice.push_str(&format!(
            " Rough inline capacity illustration, not a timing estimate: at about {SCOPE_ESTIMATE_BYTES_PER_TOKEN} bytes per token, ~{megabytes:.1} MB uses three quarters of this model's compaction/context limit. Actual headroom depends on existing context and how tool output is projected."
        ));
    } else {
        notice.push_str(
            " The model context limit is unknown; no inline capacity estimate is available.",
        );
    }
    notice.push_str(
        " Context pressure may be relieved by retiring exactly recoverable consumed output or by model summarization that can lose detail. Not every relief route summarizes; duration is unknown without measurements for the active route.",
    );
    notice.push_str(&format!(
        " As a spawned agent, cover exactly the files or ranges your assignment names and never silently substitute a sample. Your context budget is {MAX_SUBAGENT_CONTEXT_PRESSURE_EVENTS} counted context-pressure events per turn: when the assigned scope needs more, stop at the budget and report what you read with the unread files or byte ranges listed, so the parent can split the remaining work. Report unfinished coverage in the final answer."
    ));
    notice
}

async fn record_sampling_notices(
    sess: &Session,
    turn: &TurnContext,
    mcp_unavailable: bool,
    forced_terminal: Option<ForcedTerminalReason>,
) -> CodexResult<()> {
    // Root scope comes from the actual request and configured instructions, not
    // keyword matches in quotations, prohibitions, reports or rejected input.
    if matches!(
        turn.session_source,
        SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn { .. })
    ) {
        record_context_notice_if_changed(
            sess,
            turn,
            "exhaustive_scope_feasibility",
            &spawned_agent_context_budget_notice(turn),
        )
        .await?;
    }
    if mcp_unavailable {
        record_context_notice_if_changed(
            sess,
            turn,
            "mcp_catalog_notice",
            "The MCP tool catalog was temporarily unavailable when this notice was recorded. This does not mean no MCP tools are configured and is not evidence of availability on later requests. Continue independent local work; if needed, retry discovery on a subsequent step or report the unavailable capability.",
        ).await?;
    }
    if let Some(reason) = forced_terminal {
        let directive = match reason {
            ForcedTerminalReason::GenerationBudget => {
                LOGICAL_GENERATION_BUDGET_FORCED_TERMINAL_DIRECTIVE
            }
            ForcedTerminalReason::SubagentContextBudget => {
                SUBAGENT_CONTEXT_BUDGET_FORCED_TERMINAL_DIRECTIVE
            }
        };
        record_context_notice_if_changed(
            sess,
            turn,
            "forced_terminal_notice",
            &format!("Applies only to the final synthesis in turn {}. Later user input may resume work.\n{directive}", turn.sub_id),
        ).await?;
    }
    Ok(())
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
        format!("This turn exhausted its explicit safety budget of {MAX_REGULAR_LOGICAL_GENERATIONS} generations and one final summary. Work is suspended before all requested work completed. Send another message to resume."),
    )
    .await;
}

async fn admit_regular_follow_up(
    sess: &Session,
    turn_context: &TurnContext,
    budget: &LogicalGenerationBudget,
    exhaustion_reported: &mut bool,
) -> bool {
    if *exhaustion_reported || !budget.can_admit(/*terminal_requested*/ false) {
        report_logical_generation_budget_exhausted(sess, turn_context, exhaustion_reported).await;
        false
    } else {
        true
    }
}

#[cfg(test)]
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

async fn record_soft_convergence_directive(
    sess: &Session,
    turn_context: &TurnContext,
    turn_execution: &mut TurnExecutionControl,
    is_continuation: bool,
) -> std::io::Result<()> {
    if let Some(directive) = turn_execution.batching_advisory(is_continuation) {
        record_turn_execution_directive(sess, turn_context, turn_execution, directive, false)
            .await?;
    }
    if let Some(directive) = turn_execution.take_soft_convergence_directive(is_continuation) {
        let mut decision = SamplingConvergenceDecision {
            directive: Some(directive),
            ..SamplingConvergenceDecision::default()
        };
        record_convergence_decision(sess, turn_context, turn_execution, Some(&mut decision))
            .await?;
    }
    Ok(())
}

async fn record_convergence_decision(
    sess: &Session,
    turn_context: &TurnContext,
    turn_execution: &mut TurnExecutionControl,
    decision: Option<&mut SamplingConvergenceDecision>,
) -> std::io::Result<()> {
    let (proven_loop_activated, directive) = take_convergence_observation(decision);
    if proven_loop_activated {
        turn_context
            .turn_timing_state
            .record_proven_loop_activation();
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: "A repeated action/result cycle was detected. Existing results will be reused where possible; other actions remain available to complete the task.".to_string(),
            }),
        )
        .await;
    }
    let Some(directive) = directive else {
        return Ok(());
    };
    record_turn_execution_directive(sess, turn_context, turn_execution, directive, true).await
}

async fn record_turn_execution_directive(
    sess: &Session,
    turn_context: &TurnContext,
    turn_execution: &mut TurnExecutionControl,
    directive: String,
    no_progress: bool,
) -> std::io::Result<()> {
    if !turn_execution.admit_directive(&directive) {
        return Ok(());
    }
    if no_progress {
        turn_context
            .turn_timing_state
            .record_no_progress_directive();
    }
    let directive_item = ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::with_suffix(
            "msg_turn_advice",
            &turn_context.sub_id,
        )),
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "Turn-local convergence advice for task {} only; expires when that task ends. It does not constrain a later task or its scope.\n{directive}",
                turn_context.sub_id
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    // The turn-local convergence controller owns warning cadence. Session-wide
    // text deduplication would hide the same warning in every later turn.
    sess.record_conversation_items(turn_context, &[directive_item])
        .await?;
    Ok(())
}

struct CompletionStopHookReport {
    assessment: codex_protocol::protocol::TurnCompletionAssessment,
    mutation_receipt: Option<crate::tools::parallel::WorkspaceHookMutationReceipt>,
    should_block: bool,
    continuation_prompt: Option<ResponseItem>,
    continuation_feedback: Vec<String>,
    should_stop: bool,
    stop_reason: Option<String>,
}

fn completion_verification_warning(
    assessment: &codex_protocol::protocol::TurnCompletionAssessment,
) -> Option<String> {
    // Final prose is not an evidence owner: quoted historical failures and
    // wording changes cannot create or discharge a mechanical verification gap.
    assessment.verification_warning()
}

async fn run_completion_stop_hook<
    F: Future<Output = codex_protocol::protocol::TurnCompletionAssessment>,
>(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    stop_hook_active: bool,
    last_agent_message: Option<String>,
    make_assessment: impl FnOnce() -> F,
    tracker: &SharedTurnDiffTracker,
    cancellation: &CancellationToken,
) -> CompletionStopHookReport {
    // Completion is a separate, cold phase. Construct and poll its analysis
    // here rather than reserving those temporaries while HTTP is being polled.
    // The same caller still owns the work, cancellation, and persistence guards.
    let mut assessment = Box::pin(make_assessment()).await;
    let mut mutation_receipt = None;
    let event = match &turn_context.session_source {
        SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            ..
        }) => Some(codex_hooks::HookEventName::SubagentStop),
        SessionSource::SubAgent(_) => None,
        _ => Some(codex_hooks::HookEventName::Stop),
    };
    // Keep no-handler completion free of a workspace lease, task or scan.
    let stop = if event.is_some_and(|event| sess.hooks().has_handler_for(event)) {
        let revision = tracker.lock().await.current_mutation_revision();
        let hook_session = Arc::clone(sess);
        let hook_turn = Arc::clone(turn_context);
        let result = crate::tools::parallel::run_workspace_hook_with_receipt(
            Arc::clone(sess),
            Arc::clone(turn_context),
            Arc::clone(tracker),
            cancellation.clone(),
            async move {
                run_turn_stop_hooks(
                    &hook_session,
                    &hook_turn,
                    stop_hook_active,
                    last_agent_message,
                )
                .await
            },
        )
        .await;
        if tracker.lock().await.current_mutation_revision() != revision {
            // Arbitrary commands can leave descendants behind. An immediate
            // filesystem reread cannot certify their final effects are over.
            assessment.verification_gaps.push("A Stop/SubagentStop command entered after validation. Its workspace effects and descendant lifetime are unbounded; earlier validation does not verify the final source. The hook was not replayed.".to_string());
        }
        match result {
            Ok((observed, receipt)) => {
                mutation_receipt = receipt;
                observed.stop
            }
            Err(error) => codex_hooks::StopOutcome {
                should_block: true,
                block_reason: Some(error.to_string()),
                ..Default::default()
            },
        }
    } else {
        run_turn_stop_hooks(sess, turn_context, stop_hook_active, last_agent_message)
            .await
            .stop
    };
    if let Some(message) = completion_verification_warning(&assessment) {
        // A truthful limitation is allowed, but must not silently look like
        // verified task completion. Do not auto-loop on a genuine blocker.
        sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
            .await;
    }
    if !assessment.advisories.is_empty() {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Completion advisory: {}", assessment.advisories.join(" ")),
            }),
        )
        .await;
    }
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
                    "Stop hook blocked completion without actionable feedback{reason}."
                ),
            }),
        )
        .await;
    }
    CompletionStopHookReport {
        assessment,
        mutation_receipt: mutation_receipt.filter(|_| !stop.should_block && !stop.should_stop),
        should_block: stop.should_block,
        continuation_prompt,
        continuation_feedback: stop
            .continuation_fragments
            .iter()
            .map(|fragment| fragment.text.clone())
            .collect(),
        should_stop: stop.should_stop,
        stop_reason: stop.stop_reason,
    }
}

#[instrument(level = "trace", skip_all)]
async fn turn_diff_display_roots(
    sess: &Session,
    turn_context: &TurnContext,
) -> Vec<(String, PathBuf)> {
    let mut roots = sess
        .services
        .git_workspace
        .snapshot(&turn_context.environments)
        .await
        .display_roots();
    for selection in turn_context.environments.to_selections() {
        if !roots.iter().any(|(id, _)| id == &selection.environment_id) {
            roots.push((
                selection.environment_id,
                PathBuf::from(selection.cwd.inferred_native_path_string()),
            ));
        }
    }
    roots
}

struct RecordedInputOutcome {
    should_stop: bool,
    accepted_context_input: bool,
    instructions_changed: bool,
    renews_generation_budget: bool,
}

/// Extraction and recorder registration share the expected active-task lock.
#[expect(
    clippy::await_holding_invalid_type,
    reason = "input transfer and recorder enrollment must be atomic with terminal claim"
)]
async fn drain_and_record_pending_inputs(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    initial_mailbox_only: bool,
    initial_input: &[TurnInput],
) -> CodexResult<RecordedInputOutcome> {
    let completion = {
        let mut active = sess.active_turn.lock().await;
        let active = active
            .as_mut()
            .filter(|active| {
                active
                    .task
                    .as_ref()
                    .is_some_and(|task| task.turn_context.sub_id == turn_context.sub_id)
            })
            .ok_or(CodexErr::TurnAborted)?;
        let state = Arc::clone(&active.turn_state);
        let task = active.task.as_mut().ok_or(CodexErr::TurnAborted)?;
        let input = if initial_mailbox_only {
            sess.input_queue
                .take_initial_mailbox_for_state(Some(&state), initial_input, true)
                .await
        } else {
            sess.input_queue
                .take_pending_for_state(Some(&state), true)
                .await
        };
        if input.is_empty() {
            return Ok(RecordedInputOutcome {
                should_stop: false,
                accepted_context_input: false,
                instructions_changed: false,
                renews_generation_budget: false,
            });
        }
        let owner = Arc::clone(sess);
        let turn = Arc::clone(turn_context);
        let batch = input;
        let cancellation = task.auxiliary_cancellation_token.child_token();
        let (sent, completion) = tokio::sync::oneshot::channel();
        task.auxiliary_tasks.spawn(async move {
            let result =
                inspect_and_record_owned_input(&owner, &turn, batch, &cancellation, Some(state))
                    .await;
            let _ = sent.send(result);
        });
        completion
    };
    let recorded = completion
        .await
        .map_err(|error| CodexErr::Fatal(format!("pending input recorder failed: {error}")))??;
    Ok(recorded)
}

/// The per-turn owner keeps the uncommitted suffix available to its finalizer.
async fn inspect_and_record_owned_input(
    owner: &Arc<Session>,
    turn: &Arc<TurnContext>,
    input: Vec<TurnInput>,
    cancellation: &CancellationToken,
    state: Option<Arc<tokio::sync::Mutex<crate::state::TurnState>>>,
) -> CodexResult<RecordedInputOutcome> {
    let inspected = inspect_inputs(owner, turn, &input)
        .or_cancel(cancellation)
        .await
        .map_err(|_| CodexErr::TurnAborted)?;
    if cancellation.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if let Some(state) = &state {
        state.lock().await.input_in_flight = inspected.items.iter().cloned().collect();
    }
    for (index, item) in inspected.items.iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        // Accepted appends keep their existing ordered commit owner. Never
        // restore a committed prefix when only its sampling waiter is dropped.
        if let Err(error) = record_pending_input(owner, turn, item.clone(), Vec::new()).await {
            if state.is_none() {
                owner
                    .input_queue
                    .restore_transferred_startup_input(inspected.items[index..].to_vec())
                    .await;
            }
            return Err(CodexErr::from(error));
        }
        if let Some(state) = &state {
            state.lock().await.input_in_flight.pop_front();
        }
    }
    Ok(inspected.outcome)
}

#[cfg(test)]
async fn run_hooks_and_record_inputs_detailed(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: Vec<TurnInput>,
) -> CodexResult<RecordedInputOutcome> {
    let owner = Arc::clone(sess);
    let turn = Arc::clone(turn_context);
    sess.terminal_tasks
        .spawn(async move {
            inspect_and_record_owned_input(&owner, &turn, input, &CancellationToken::new(), None)
                .await
        })
        .await
        .map_err(|error| CodexErr::Fatal(format!("pending input recording failed: {error}")))?
}

fn resets_turn_execution(input: &TurnInput) -> bool {
    match input {
        TurnInput::UserInput { content, .. } => !content.is_empty(),
        TurnInput::ResponseItem(_)
        | TurnInput::InterAgentCommunication(_)
        | TurnInput::AdditionalContext(_) => true,
        TurnInput::InternalResponseItem(_) => false,
    }
}

/// New instructions renew the turn's generation budget: user input, steering,
/// and mail from the agent's own owner. Reports from children or peers only
/// add context; if they renewed it, a coordinator receiving a stream of child
/// results would never reach the forced synthesis.
fn renews_generation_budget(input: &TurnInput) -> bool {
    match input {
        TurnInput::InterAgentCommunication(mail) => mail
            .recipient
            .as_str()
            .strip_prefix(mail.author.as_str())
            .is_some_and(|descendant| descendant.starts_with('/')),
        input => resets_turn_execution(input),
    }
}

struct InspectedInputs {
    items: Vec<TurnInput>,
    outcome: RecordedInputOutcome,
}

async fn record_inspected_inputs(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
) -> CodexResult<()> {
    for item in input {
        record_pending_input(sess, turn_context, item.clone(), Vec::new()).await?;
        if let Some(state) = sess
            .input_queue
            .turn_state_for_sub_id(&sess.active_turn, &turn_context.sub_id)
            .await
        {
            let mut state = state.lock().await;
            if state.input_in_flight.front() == Some(item) {
                state.input_in_flight.pop_front();
            }
        }
    }
    Ok(())
}

async fn inspect_inputs(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[TurnInput],
) -> InspectedInputs {
    let mut items = Vec::new();
    let mut blocked_input = false;
    let mut accepted_context_input = false;
    let mut renews_budget = false;
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
        } else {
            if resets_turn_execution(input_item) {
                accepted_context_input = true;
            }
            renews_budget |= renews_generation_budget(input_item);
            items.push(input_item.clone());
        }
        items.extend(
            prepare_additional_context_items(sess, turn_context, hook_outcome.additional_contexts)
                .await
                .into_iter()
                .map(TurnInput::InternalResponseItem),
        );
    }
    InspectedInputs {
        items,
        outcome: RecordedInputOutcome {
            should_stop: blocked_input && !accepted_context_input,
            accepted_context_input,
            // Owner/ancestor instructions and user/context steering reset
            // passive evidence; reports still invalidate completion and caches.
            instructions_changed: renews_budget,
            renews_generation_budget: renews_budget,
        },
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

    // Only named candidates belong in stable context. Broad discovery belongs
    // to tool_search; incidental words in coding tasks must not pin the catalog.
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
    plugins_config: &PluginsConfigInput,
    loaded_plugins: &PluginLoadOutcome,
    user_input: &[ContentItem],
) -> Vec<ResponseItem> {
    if !tool_suggest_enabled(turn_context) {
        return Vec::new();
    }

    let auth = sess.services.auth_manager.auth().await;
    let Some(candidates) = sess
        .services
        .plugins_manager
        .cached_recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
            plugins_config,
            loaded_plugins,
            auth: auth.as_ref(),
            disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
            app_server_client_name: turn_context.app_server_client_name.as_deref(),
        })
    else {
        return Vec::new();
    };
    let candidates = task_relevant_recommended_plugins(user_input, candidates);
    RecommendedPluginsInstructions::from_plugins(candidates)
        .map(ContextualUserFragment::into)
        .into_iter()
        .collect()
}

fn time_planning_step<T, F: Future<Output = T>>(
    step: &'static str,
    make_work: impl FnOnce() -> F,
) -> impl Future<Output = T> {
    // Keep each child's construction temporaries out of the orchestration
    // future too, not just its adapters/join state. No task is spawned.
    async move {
        struct Timer {
            step: &'static str,
            started: std::time::Instant,
        }
        impl Drop for Timer {
            fn drop(&mut self) {
                tracing::info!(
                    planning_step = self.step,
                    elapsed_ms = self.started.elapsed().as_millis() as u64,
                    "turn planning step finished"
                );
            }
        }
        let _timer = Timer {
            step,
            started: std::time::Instant::now(),
        };
        Box::pin(make_work()).await
    }
    .instrument(trace_span!("time_planning_step"))
}

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
            TurnInput::ResponseItem(_)
            | TurnInput::AdditionalContext(_)
            | TurnInput::InternalResponseItem(_)
            | TurnInput::InterAgentCommunication(_) => None,
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

    // Read-only DAG roots P and E are independent. Extension contributors poll
    // concurrently internally and collect by registration index.
    let plugins_config_input = turn_context.config.plugins_config_input();
    let (loaded_plugins, extension_injection_items) = tokio::join!(
        time_planning_step("plugins", || sess
            .services
            .plugins_manager
            .plugins_for_config(&plugins_config_input)),
        time_planning_step("extension_input", || build_extension_turn_input_items(
            sess,
            step_context.as_ref(),
            &user_input,
            cancellation_token
        ))
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
        time_planning_step("recommendations", || {
            build_recommended_plugin_items(
                sess,
                turn_context,
                &plugins_config_input,
                &loaded_plugins,
                &recommended_plugin_input,
            )
        }),
        time_planning_step("mcp_catalog", || async {
            if turn_context.apps_enabled() || !mentioned_plugins.is_empty() {
                step_context.mcp_tools().or_cancel(cancellation_token).await
            } else {
                Ok(&[][..])
            }
        }),
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
    let injected_host_skill_prompts = turn_context
        .extension_data
        .get::<InjectedHostSkillPrompts>();
    let mentioned_skills = injected_host_skill_prompts
        .as_ref()
        .map(|prompts| prompts.selected.clone())
        .unwrap_or_else(|| {
            collect_explicit_skill_mentions(
                &user_input,
                &skills_outcome.skills,
                &skills_outcome.disabled_paths,
                &connector_slug_counts,
            )
        });
    let dependency_skills = injected_host_skill_prompts
        .as_ref()
        .map(|prompts| prompts.admitted_skills())
        .unwrap_or_else(|| mentioned_skills.clone());
    // Once SK is resolved, inventory-effect planning and pure skill materialization
    // are independent and remain side-effect free.
    let (planned_mcp, skill_plan) = tokio::join!(
        time_planning_step("mcp_dependencies", || plan_mcp_dependencies(
            sess,
            turn_context,
            &dependency_skills
        )),
        time_planning_step("skill_injections", || plan_skill_injections_with_resolved(
            &mentioned_skills,
            Some(skills_outcome),
            injected_host_skill_prompts.as_deref()
        ))
    );
    let skill_connector_items = skill_plan
        .injections
        .items
        .iter()
        .map(|skill| {
            ContextualUserFragment::into(RenderedContextFragment::new(skill.role(), skill.render()))
        })
        .collect::<Vec<_>>();
    let skill_connector_ids = collect_explicit_app_ids_from_skill_items(
        &skill_connector_items,
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

    let mut injection_items =
        build_bounded_skill_context_items(skill_plan.injections.items.iter().filter(|skill| {
            injected_host_skill_prompts
                .as_ref()
                .is_none_or(|prompts| !prompts.contains_path(&skill.path))
        }));
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
        time_planning_step("tool_router", || built_tools_for_pending_turn(
            sess.as_ref(),
            &step_context,
            &skill_plan.invocations,
            planning_generation,
            cancellation_token,
        )),
        time_planning_step("context_update", || sess
            .prepare_context_update(step_context.as_ref())),
    );
    let first_router = first_router?;
    if sess.services.planning_generation() != planning_generation {
        return Ok(PendingTurnPlanBuild::Stale);
    }
    let mut warnings = planned_mcp.warnings;
    warnings.extend(first_router.planning_warnings().iter().cloned());
    warnings.extend(skill_plan.injections.warnings.iter().cloned());
    let initial_context = !sess.has_reference_context_item().await;
    let mut pending_token_estimate = estimate_pending_tokens(
        input,
        &injection_items,
        prepared_context_update.context_items(),
        first_router.as_ref(),
        initial_context,
    );
    let context_tokens = estimate_pending_additional_context_tokens(sess, input).await;
    pending_token_estimate.total_tokens = pending_token_estimate.total_tokens.saturating_add(
        crate::client_common::output_schema_tokens(
            turn_context
                .final_output_json_schema
                .as_ref()
                .map(|schema| schema.original()),
            true,
        ),
    );
    pending_token_estimate.total_tokens = pending_token_estimate
        .total_tokens
        .saturating_add(context_tokens);
    pending_token_estimate.body_growth_tokens = pending_token_estimate
        .body_growth_tokens
        .saturating_add(context_tokens);
    let projected_prompt_pressure = time_planning_step("prompt_pressure", || {
        projected_prompt_pressure(sess, turn_context, pending_token_estimate)
    })
    .await;
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

fn stabilize_pending_turn_plan<'a>(
    sess: &'a Arc<Session>,
    turn_context: &'a Arc<TurnContext>,
    input: &'a [TurnInput],
    client_session: &'a mut ModelClientSession,
    planning_iterations: &'a mut usize,
    completed_mcp_effect: &'a mut Option<(String, Option<HashSet<String>>)>,
    cancellation_token: &'a CancellationToken,
) -> impl std::future::Future<Output = CodexResult<PendingTurnPlan>> + 'a {
    Box::pin(async move {
        let mut check_previous_model_compaction = true;
        let mut incoming_precompaction_completed = false;
        let mut last_retry_reason = "retry budget was exhausted before this planning invocation";
        loop {
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            // Charge every attempt before maintenance or snapshot work, including
            // retries caused by compaction and stale generations.
            advance_pending_turn_plan_iteration(planning_iterations, last_retry_reason)
                .map_err(|message| planning_failure_with_timing(turn_context, message))?;

            // Model-transition compaction is independent of pending input. Context-pressure
            // compaction waits for the absolute next-prompt projection built below.
            let history_compaction = time_planning_step("history_compaction", || {
                run_history_pre_sampling_compact(
                    sess,
                    turn_context,
                    client_session,
                    check_previous_model_compaction,
                    cancellation_token,
                )
            })
            .await?;
            check_previous_model_compaction = false;
            if history_compaction.is_some() {
                last_retry_reason = "history compaction invalidated the plan";
                client_session.invalidate_incremental_history("compaction");
                turn_context
                    .turn_timing_state
                    .record_planning_invalidation();
                continue;
            }

            if let Some((effect_id, Some(expected_inventory_keys))) = completed_mcp_effect
                && !inventory_contains_expected(sess, expected_inventory_keys).await
            {
                return Err(planning_failure_with_timing(
                    turn_context,
                    format!(
                        "completed inventory effect `{effect_id}` is missing its expected model-visible state"
                    ),
                ));
            }

            let planning_generation = sess.services.planning_generation();
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            // Capturing a step can publish an MCP manager. Let that resource-owning
            // operation finish; the pure build below checks cancellation again.
            let step_context = time_planning_step("step_context", || {
                sess.capture_step_context(Arc::clone(turn_context))
            })
            .await?;
            let plan_build = Box::pin(build_pure_pending_turn_plan(
                sess,
                step_context,
                input,
                planning_generation,
                cancellation_token,
            ))
            .or_cancel(cancellation_token)
            .await??;
            let plan = match plan_build {
                PendingTurnPlanBuild::Stale => {
                    last_retry_reason = "planning generation changed while building the plan";
                    continue;
                }
                PendingTurnPlanBuild::Ready(plan) => *plan,
            };
            turn_context
                .turn_timing_state
                .record_planning_fixed_point_iteration();

            // Compaction is maintenance of the pre-existing history, not persistence
            // of this pending turn. Any compaction invalidates this pure plan and loops
            // through a fresh snapshot before effects are applied.
            let compaction_reason = time_planning_step("pending_input_compaction", || {
                run_pending_input_pre_sampling_compact(
                    sess,
                    turn_context,
                    client_session,
                    Arc::clone(&plan.step_context),
                    plan.projected_prompt_pressure,
                    !incoming_precompaction_completed,
                    cancellation_token,
                )
            })
            .await?;
            if compaction_reason.is_some() {
                last_retry_reason = "pending-input compaction invalidated the plan";
                incoming_precompaction_completed = true;
                client_session.invalidate_incremental_history("compaction");
                turn_context
                    .turn_timing_state
                    .record_planning_invalidation();
                continue;
            }

            if let Some(effect) = plan.mcp_dependency_effect.as_ref()
                && !mcp_dependency_effect_is_completed(completed_mcp_effect.as_ref(), &effect.id)
            {
                let outcome = time_planning_step("mcp_effect", || {
                    apply_mcp_dependency_effect(sess, turn_context, cancellation_token, effect)
                })
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
                    last_retry_reason = "MCP dependency installation changed the inventory";
                    client_session.invalidate_incremental_history("model-visible planning effect");
                    turn_context
                        .turn_timing_state
                        .record_planning_invalidation();
                    require_newer_planning_generation(
                        &effect.id,
                        plan.planning_generation,
                        sess.services.planning_generation(),
                    )
                    .map_err(|message| planning_failure_with_timing(turn_context, message))?;
                    continue;
                }
            }

            if sess.services.planning_generation() != plan.planning_generation {
                last_retry_reason = "planning generation changed before committing the plan";
                turn_context
                    .turn_timing_state
                    .record_planning_invalidation();
                continue;
            }
            return Ok(plan);
        }
    })
}

const MAX_PENDING_TURN_PLAN_ITERATIONS: usize = 8;

fn advance_pending_turn_plan_iteration(
    iterations: &mut usize,
    last_retry_reason: &str,
) -> Result<(), String> {
    *iterations = iterations.saturating_add(1);
    if *iterations > MAX_PENDING_TURN_PLAN_ITERATIONS {
        return Err(format!(
            "pending-turn planning did not stabilize after {MAX_PENDING_TURN_PLAN_ITERATIONS} iterations; last retry: {last_retry_reason}"
        ));
    }
    Ok(())
}

fn mcp_dependency_effect_is_completed(
    completed_effect: Option<&(String, Option<HashSet<String>>)>,
    effect_id: &str,
) -> bool {
    completed_effect.is_some_and(|(completed_id, _)| completed_id == effect_id)
}

fn require_newer_planning_generation(
    effect_id: &str,
    before_generation: u64,
    after_generation: u64,
) -> Result<(), String> {
    if after_generation <= before_generation {
        return Err(format!(
            "inventory effect `{effect_id}` completed at planning generation {before_generation}, but the next observable generation {after_generation} did not advance"
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
        // Observational enrichment must not delay sampling or retain an
        // unbounded number of plugin inventories under blocking-pool pressure.
        static ENRICHMENT_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
        let Ok(permit) = ENRICHMENT_SLOTS.try_acquire() else {
            warn!("plugin mention telemetry enrichment capacity exhausted");
            return;
        };
        let plugins_manager = Arc::clone(&sess.services.plugins_manager);
        let mentioned_plugins = plan.mentioned_plugins.clone();
        let analytics = sess.services.analytics_events_client.clone();
        let tracking = plan.tracking.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            for plugin in mentioned_plugins.iter().filter_map(|summary| {
                plugins_manager.telemetry_metadata_for_capability_summary(summary)
            }) {
                analytics.track_plugin_used(tracking.clone(), plugin);
            }
        });
        turn_context
            .turn_timing_state
            .record_planning_semantic_effect();
    }
}

fn planning_failure(message: impl Into<String>) -> CodexErr {
    CodexErr::Fatal(format!("pending-turn planning failure: {}", message.into()))
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
    has_real_user_boundary: bool,
}

async fn estimate_pending_additional_context_tokens(sess: &Session, input: &[TurnInput]) -> i64 {
    let maps = input
        .iter()
        .filter_map(|item| match item {
            TurnInput::AdditionalContext(values) => Some(values),
            _ => None,
        })
        .collect::<Vec<_>>();
    if maps.is_empty() {
        return 0;
    }
    let mut preview = {
        let state = sess.state.lock().await;
        if maps
            .iter()
            .all(|values| state.additional_context.is_current(values))
        {
            return 0;
        }
        state.additional_context.clone()
    };
    maps.into_iter()
        .flat_map(|values| preview.merge(values.clone()))
        .map(ResponseItem::from)
        .map(|item| crate::context_manager::estimate_item_token_count(&item))
        .fold(0i64, i64::saturating_add)
}

fn estimate_pending_tokens(
    input: &[TurnInput],
    injection_items: &[ResponseItem],
    context_update_items: &[ResponseItem],
    router: &ToolRouter,
    initial_context: bool,
) -> PendingTokenEstimate {
    use crate::context_manager::estimate_item_token_count;
    let is_real_user_boundary = |item: &ResponseItem| {
        matches!(item, ResponseItem::Message { role, .. } if role == "user")
            && crate::context_manager::is_user_turn_boundary(item)
    };
    let mut has_real_user_boundary = false;
    let input_tokens = input.iter().fold(0i64, |tokens, item| {
        let item_tokens = match item {
            TurnInput::UserInput { content, .. } => {
                // Estimate local images without doing filesystem/image processing during
                // admission. Inline images retain their metadata for the shared estimator.
                let content: Vec<UserInput> = content
                    .iter()
                    .map(|item| match item {
                        UserInput::LocalImage { detail, .. } => UserInput::Image {
                            image_url: "data:image/png;base64,".to_string(),
                            detail: *detail,
                        },
                        item => item.clone(),
                    })
                    .collect();
                let item =
                    ResponseItem::from(codex_protocol::models::ResponseInputItem::from(content));
                has_real_user_boundary |= matches!(
                    &item, ResponseItem::Message { content, .. } if !content.is_empty()
                ) && is_real_user_boundary(&item);
                estimate_item_token_count(&item)
            }
            TurnInput::ResponseItem(item) => {
                has_real_user_boundary |= is_real_user_boundary(item);
                estimate_item_token_count(item)
            }
            TurnInput::InternalResponseItem(item) => estimate_item_token_count(item),
            // The source owner estimates the actual ordered delta, not the
            // supplied map (which can be unchanged and deliver nothing).
            TurnInput::AdditionalContext(_) => 0,
            TurnInput::InterAgentCommunication(communication) => {
                estimate_item_token_count(&communication.to_model_input_item())
            }
        };
        tokens.saturating_add(item_tokens)
    });
    let mut injection_tokens = 0i64;
    let mut body_injection_tokens = 0i64;
    for item in injection_items {
        let tokens = estimate_item_token_count(item);
        injection_tokens = injection_tokens.saturating_add(tokens);
        let stable = matches!(item, ResponseItem::Message { role, content, .. }
            if role == "user" && !content.is_empty()
            && content.iter().all(is_startup_contextual_user_fragment));
        if !stable {
            body_injection_tokens = body_injection_tokens.saturating_add(tokens);
        }
    }
    let context_update_tokens = context_update_items.iter().fold(0i64, |tokens, item| {
        tokens.saturating_add(estimate_item_token_count(item))
    });
    let tool_tokens = i64::try_from(
        router
            .model_visible_schemas()
            .serialized()
            .len()
            .div_ceil(4),
    )
    .unwrap_or(i64::MAX);
    PendingTokenEstimate {
        total_tokens: input_tokens
            .saturating_add(injection_tokens)
            .saturating_add(context_update_tokens)
            .saturating_add(tool_tokens),
        body_growth_tokens: input_tokens
            .saturating_add(body_injection_tokens)
            .saturating_add(if initial_context {
                0
            } else {
                context_update_tokens
            }),
        has_real_user_boundary,
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
            sess.get_context_tokens_for_turn(turn_context),
            async {
                if pending_token_estimate.has_real_user_boundary {
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
    let calibrated_local_prompt = sess
        .calibrated_prompt_tokens(
            turn_context,
            committed_history_tokens.saturating_add(pending_token_estimate.total_tokens),
        )
        .await;
    // Calibration changes only the local estimate, never the measured-usage
    // floor or the configured compaction/context-window limits.
    let total_tokens = projected_prompt_tokens_from_estimates(
        active_context_tokens,
        calibrated_local_prompt,
        0,
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

/// Recorded final-answer requests used at most ~1.5K output and reasoning tokens.
const FINAL_ANSWER_TOKEN_RESERVE: i64 = 8_192;

/// A completed plan, or a next request that is already the tool-free final
/// synthesis, means the next request is most likely the final answer.
/// Compacting first spends a summarization request (96 s in a recorded turn)
/// only to answer from a summary. Defer once while the answer still fits:
/// another tool call compacts on the next check, and a provider overflow still
/// compacts and retries. `active_context_tokens` is server usage plus the
/// unsent tail, not the local whole-prompt estimate.
fn defer_compaction_for_final_answer(
    final_answer_expected: bool,
    active_context_tokens: i64,
    context_window: Option<i64>,
) -> bool {
    final_answer_expected
        && context_window.is_some_and(|window| {
            active_context_tokens.saturating_add(FINAL_ANSWER_TOKEN_RESERVE) <= window
        })
}

fn build_bounded_skill_context_items<'a>(
    skill_fragments: impl IntoIterator<Item = &'a codex_core_skills::injection::SkillInjection>,
) -> Vec<ResponseItem> {
    use sha2::Digest;
    let fragments = skill_fragments.into_iter().collect::<Vec<_>>();
    let notices = fragments.iter().map(|skill| {
        let mut notice = (*skill).clone();
        // An undelivered replacement cannot retire successfully delivered rules.
        notice.source_id = None;
        notice.contents = "INCOMPLETE MODEL DELIVERY: instructions were loaded by the host but not delivered. Before using this selected skill, use read_file with the exact path above to read the complete instructions. Do not infer permission from an excerpt.".to_string();
        notice.render()
    }).collect::<Vec<_>>();
    let mut budget = ModelContextBudget::default();
    // Reserve every exact recovery identity before admitting instruction bodies.
    // Recovery notices are never cut, even for an unusually large selection.
    if !budget.try_take_bytes(notices.iter().map(String::len).sum()) {
        budget = ModelContextBudget::new(0);
    }
    fragments
        .into_iter()
        .zip(notices)
        .map(|(fragment, notice)| {
            let full = fragment.render();
            let complete = full.len() <= notice.len()
                || budget.try_take_bytes(full.len() - notice.len());
            let text =
                if complete {
                    full
                } else {
                    notice
                };
            let mut item: ResponseItem = ContextualUserFragment::into(
                RenderedContextFragment::new(fragment.role(), text),
            );
            if complete && let Some(source) = &fragment.source_id {
                let revision = hex::encode(sha2::Sha256::digest(fragment.contents.as_bytes()));
                if let ResponseItem::Message { id, .. } = &mut item {
                    *id = Some(ResponseItemId::new(&format!("msg_sctx_skill_revision_v1_{source}_{revision}")));
                }
            }
            item
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
        bound_selected_capability_roots: step_context.selected_capability_roots.clone(),
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
        let input = &input;
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
    let mut fragments = Vec::new();
    while let Some(contributed_fragments) = pending.next().await {
        let contributed_fragments = contributed_fragments?;
        fragments.extend(
            contributed_fragments
                .into_iter()
                .map(|fragment| (fragment.role(), fragment.render())),
        );
    }
    let mut budget = ModelContextBudget::default();
    let mut items = Vec::new();
    let total = fragments.iter().map(|(_, text)| text.len()).sum::<usize>();
    let notice = if total > budget.remaining_bytes() {
        let recovery = sess
            .retain_context_source(
                "selected_extension_instructions",
                serde_json::json!(
                    fragments
                        .iter()
                        .map(|(role, text)| serde_json::json!({"role":role,"text":text}))
                        .collect::<Vec<_>>()
                ),
            )
            .await?;
        let text = format!(
            "Selected extension instructions: INCOMPLETE MODEL DELIVERY. Whole fragments follow where admitted; others (including producer recovery notices) remain in this exact directory. Recover /items before using selected capabilities. Loading is not delivery; recovered entries retain their recorded authority. {recovery}"
        );
        budget.try_take(&text);
        Some(ContextualUserFragment::into(RenderedContextFragment::new(
            "developer",
            text,
        )))
    } else {
        None
    };
    for (role, text) in fragments {
        if budget.try_take(&text) {
            items.push(ContextualUserFragment::into(RenderedContextFragment::new(
                role, text,
            )));
        }
    }
    items.extend(notice);
    if let Some(selected) = turn_context
        .extension_data
        .get::<InjectedHostSkillPrompts>()
    {
        let mut selected = (*selected).clone();
        selected.retain_admitted_items(&items);
        turn_context.extension_data.insert(selected);
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
    let (thread_config, is_first_turn) = {
        let mut state = sess.state.lock().await;
        (
            state.session_configuration.thread_config_snapshot(),
            state.take_next_turn_is_first(),
        )
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
                    TurnInput::ResponseItem(_)
                    | TurnInput::AdditionalContext(_)
                    | TurnInput::InternalResponseItem(_)
                    | TurnInput::InterAgentCommunication(_) => None,
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

#[instrument(level = "trace", skip_all)]
async fn run_history_pre_sampling_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    check_previous_model: bool,
    cancellation_token: &CancellationToken,
) -> CodexResult<Option<PreSamplingCompactionReason>> {
    if check_previous_model
        && maybe_run_previous_model_inline_compact(
            sess,
            turn_context,
            client_session,
            cancellation_token,
        )
        .await?
    {
        return Ok(Some(PreSamplingCompactionReason::PreviousModel));
    }
    Ok(None)
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
    if !token_status.token_limit_reached {
        return Ok(None);
    }
    if !allow_pending_input_compaction {
        return Err(planning_failure_with_timing(
            turn_context,
            "the prompt still exceeds its configured token limit after compaction; stopping before another model request. Reduce attached context or start a new task",
        ));
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
        /*prefetched_world_state*/ None,
        CompactionReason::ContextLimit,
        CompactionPhase::PreTurn,
        // This trigger is the local projection of the next prompt, not a
        // provider rejection, so free retirement of consumed results is tried
        // first against that same projection.
        ConsumedOutputRetirement::Eligible {
            projected_pressure: Some(projected_prompt_pressure),
        },
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
    sess.capture_step_context(Arc::clone(turn_context))
        .await
        .ok()
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
            .await?;
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
            /*prefetched_world_state*/ None,
            CompactionReason::CompHashChanged,
            CompactionPhase::PreTurn,
            // Compaction here re-establishes compatibility with a changed
            // prompt, not window headroom; retiring outputs cannot do that.
            ConsumedOutputRetirement::Unavailable,
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
            .await?;
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
            /*prefetched_world_state*/ None,
            CompactionReason::ModelDownshift,
            CompactionPhase::PreTurn,
            // The smaller model needs a summary sized for its own window, which
            // retiring consumed results to pins does not establish.
            ConsumedOutputRetirement::Unavailable,
            cancellation_token,
        )
        .await?;
        return Ok(true);
    }
    Ok(false)
}

/// Outputs first consumed by the newest generations stay inline as the active
/// working set when consumed results are retired under context pressure.
const CONTEXT_PRESSURE_KEPT_GENERATIONS: usize = 3;
/// After the summary request failed, keep only the newest generation inline.
const FAILED_COMPACTION_KEPT_GENERATIONS: usize = 1;

/// Steady-state ceiling for proactive retirement, as a fraction of the
/// automatic-compaction limit.
///
/// `retire_consumed_tool_outputs_for_context_pressure` already retires down to
/// `limit * 2 / 3` once the limit is reached, so reusing that same fraction
/// here changes only *when* retirement runs, never which outputs qualify.
const PROACTIVE_RETIREMENT_TARGET_NUMERATOR: i64 = 2;
const PROACTIVE_RETIREMENT_TARGET_DENOMINATOR: i64 = 3;

/// Price of a cached input token as a fraction of an uncached one. OpenAI's
/// published API pricing bills cached input for the GPT-5 family at a tenth of
/// the uncached rate; the harness has no pricing for the model in use.
///
/// A model with a higher ratio would justify retiring somewhat earlier than
/// this assumes; the automatic-compaction limit still retires regardless.
const CACHED_INPUT_COST_NUMERATOR: u128 = 1;
const CACHED_INPUT_COST_DENOMINATOR: u128 = 10;

/// Retires consumed tool results above the steady-state ceiling once
/// re-sending them has cost more than the cache miss retiring them causes,
/// instead of waiting for the automatic-compaction limit.
///
/// This is the same exact, recoverable retirement the compaction path already
/// performs: `read_tool_output` still returns the original bytes for every
/// retired output, and no output is retired here that the limit-triggered
/// path would have kept. Only the trigger point differs.
///
/// Retirement replaces the oldest consumed outputs with pins, so the next
/// prompt diverges from every earlier one near the start of history. Recorded
/// sessions show the provider then reuses only the fixed prompt prefix: the
/// whole remaining history is re-read uncached, which costs `(1 - c)` per
/// token more than reading it from cache, where `c` is the cached price ratio.
/// Keeping the outputs instead costs `c` per retirable token on every request
/// that re-sends them. Each check charges the request that just completed for
/// what it re-sent, and retirement runs once those charges pay for the re-read:
/// the break-even rule for a rent-or-buy decision. With steady growth it
/// retires batches of about `sqrt(2 * (1 - c) * rewritten * growth / c)`
/// tokens, so a session that would otherwise rewrite its whole history for a
/// small saving every few requests leaves retirement to the limit instead.
///
/// Returns true when retirement ran, so the caller can refresh the token
/// status it based later decisions on.
async fn retire_consumed_tool_outputs_before_limit(
    sess: &Session,
    turn_context: &TurnContext,
) -> bool {
    let status = super::context_window::context_window_token_status(sess, turn_context).await;
    // Only the soft automatic-compaction scope defines a steady state. Without
    // it there is no limit to stay below, and the limit-triggered path remains
    // the only owner of retirement.
    let Some(limit) = status.auto_compact_scope_limit.filter(|limit| *limit > 0) else {
        return false;
    };
    let target = limit.saturating_mul(PROACTIVE_RETIREMENT_TARGET_NUMERATOR)
        / PROACTIVE_RETIREMENT_TARGET_DENOMINATOR;
    let excess =
        u64::try_from(status.auto_compact_scope_tokens.saturating_sub(target)).unwrap_or(0);
    let (call_ids, saved, carried, history) = {
        let mut state = sess.state.lock().await;
        let (call_ids, saved) = if excess == 0 {
            (Vec::new(), 0)
        } else {
            state
                .history
                .pressure_retirement_selection(CONTEXT_PRESSURE_KEPT_GENERATIONS, excess)
        };
        if call_ids.is_empty() || saved == 0 {
            // No request is re-sending retirable outputs, so no charge accrues.
            state.proactive_retirement_carried_tokens = 0;
            return false;
        }
        // The request that just completed re-sent these outputs.
        let carried = state
            .proactive_retirement_carried_tokens
            .saturating_add(saved);
        state.proactive_retirement_carried_tokens = carried;
        (call_ids, saved, carried, state.history.clone())
    };
    // Instructions and tool schemas stay cached across the rewrite; the clone
    // shares item storage, so estimating outside the lock copies nothing.
    let Some(history_tokens) = history.estimate_prepared_token_count_with_base_instructions(
        &turn_context.model_info.input_modalities,
        &BaseInstructions {
            text: String::new(),
        },
    ) else {
        return false;
    };
    let rewritten = u64::try_from(history_tokens)
        .unwrap_or(0)
        .saturating_sub(saved);
    if u128::from(carried) * CACHED_INPUT_COST_NUMERATOR
        < u128::from(rewritten) * (CACHED_INPUT_COST_DENOMINATOR - CACHED_INPUT_COST_NUMERATOR)
    {
        trace!(
            carried_tokens = carried,
            estimated_saved_tokens = saved,
            rewritten_tokens = rewritten,
            "deferring proactive context retirement until re-sending the outputs costs more than the cache miss"
        );
        return false;
    }
    let checkpoint = crate::tool_history::pressure_retirement_checkpoint(
        &call_ids,
        crate::tool_history::PressureRetirementTrigger::Proactive,
    );
    if let Err(error) = sess
        .record_conversation_items(turn_context, &[checkpoint])
        .await
    {
        // The limit-triggered path still owns this history later.
        warn!(%error, "proactive context retirement was not recorded; leaving history unchanged");
        return false;
    }
    sess.state.lock().await.proactive_retirement_carried_tokens = 0;
    let saved_tokens = i64::try_from(saved).unwrap_or(i64::MAX);
    sess.discount_retired_context_tokens(saved_tokens).await;
    sess.send_token_count_event(turn_context).await;
    info!(
        retired_outputs = call_ids.len(),
        estimated_saved_tokens = saved_tokens,
        carried_tokens = carried,
        rewritten_tokens = rewritten,
        auto_compact_scope_tokens = status.auto_compact_scope_tokens,
        target,
        "retired consumed tool outputs before reaching the compaction limit"
    );
    true
}

/// Whether a compaction may first retire consumed tool results to their
/// recovery pins, which costs no model request.
#[derive(Clone, Copy, Debug)]
enum ConsumedOutputRetirement {
    /// Retirement cannot establish that the next prompt fits. After a provider
    /// overflow the local estimate already failed, so only a real summary can
    /// be trusted to shrink the prompt.
    Unavailable,
    /// The caller triggered on a local estimate, so retirement is evaluated
    /// against that same estimate. `projected_pressure` carries the next
    /// prompt's absolute projection when the trigger used one; `None` means the
    /// caller triggered on live usage.
    Eligible {
        projected_pressure: Option<ProjectedPromptPressure>,
    },
}

/// How `run_auto_compact` brought the context back under its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextPressureRelief {
    /// Retired consumed results to exact recovery pins at the usual kept
    /// generations: the operation proactive retirement performs before the
    /// limit, which a spawned agent's compaction budget does not count.
    RoutineRetirement,
    /// Summarized or replaced history, or retired past the kept generations
    /// after a summary failed.
    Compaction,
}

/// Retires consumed tool results to their recovery pins; read_tool_output
/// still recovers their exact bytes. Returns true when that alone brings the
/// context under its automatic-compaction limit, so the LLM summary is
/// unnecessary. An insufficient candidate is left for summarization; if new
/// history arrives during publication, valid pins remain and compaction follows.
///
/// `projected_pressure` is the next prompt's absolute projection when the
/// caller triggered on it instead of on live usage. Retirement must clear the
/// same pressure its caller measured: a pre-turn caller that triggered on the
/// projected prompt would otherwise discount only live usage, report that the
/// prompt fits, and send an overflowing request whose retry is a hard planning
/// failure.
async fn retire_consumed_tool_outputs_for_context_pressure(
    sess: &Session,
    turn_context: &TurnContext,
    kept_generations: usize,
    projected_pressure: Option<ProjectedPromptPressure>,
) -> bool {
    let status = match projected_pressure {
        Some(pressure) => {
            super::context_window::projected_context_window_token_status(
                sess,
                turn_context,
                pressure.total_tokens,
                pressure.auto_compact_scope_tokens,
            )
            .await
        }
        None => super::context_window::context_window_token_status(sess, turn_context).await,
    };
    let Some(limit) = status.auto_compact_scope_limit.filter(|limit| *limit > 0) else {
        return false;
    };
    // Aim well below the limit so the next compaction is not one request away.
    let target = limit.saturating_mul(2) / 3;
    let Ok(excess) = u64::try_from(status.auto_compact_scope_tokens.saturating_sub(target)) else {
        return false;
    };
    if excess == 0 {
        return false;
    }
    let mut candidate = sess.clone_history().await;
    let (call_ids, _) = candidate.pressure_retirement_selection(kept_generations, excess);
    if call_ids.is_empty() {
        return false;
    }
    let checkpoint = crate::tool_history::pressure_retirement_checkpoint(
        &call_ids,
        crate::tool_history::PressureRetirementTrigger::ContextLimit,
    );
    let Some(before) = candidate.estimate_token_count(turn_context) else {
        return false;
    };
    candidate.record_items(
        std::iter::once(&checkpoint),
        turn_context.model_info.truncation_policy.into(),
    );
    let Some(after) = candidate.estimate_token_count(turn_context) else {
        return false;
    };
    // Size the actual pin rewrite AND its checkpoint with sampling's estimator.
    // Apply the same calibration to each side, retaining the caller's pending
    // input/scaffold pressure rather than substituting live history pressure.
    let saved = sess
        .calibrated_prompt_tokens(turn_context, before)
        .await
        .saturating_sub(sess.calibrated_prompt_tokens(turn_context, after).await);
    // Check the physical window against the same total the caller triggered on.
    // `active_context_tokens` stays live even under a projection, so using it
    // here would clear a projected prompt that still overflows.
    let full_window_tokens = projected_pressure.map_or(status.active_context_tokens, |pressure| {
        pressure.total_tokens
    });
    let fits = status.auto_compact_scope_tokens.saturating_sub(saved) < limit
        && status
            .full_context_window_limit
            .is_none_or(|full| full_window_tokens.saturating_sub(saved) < full);
    if saved <= 0 || !fits {
        return false;
    }
    if let Err(error) = sess
        .record_conversation_items(turn_context, &[checkpoint])
        .await
    {
        warn!(%error, "context-pressure retirement was not recorded; compacting instead");
        return false;
    }
    // The rewrite starts a new prefix, so proactive carrying restarts too.
    sess.state.lock().await.proactive_retirement_carried_tokens = 0;
    sess.discount_retired_context_tokens(saved).await;
    sess.send_token_count_event(turn_context).await;
    // Publication can admit new history while persistence is awaited. Do not
    // spend the caller's one relief opportunity on an overflowing committed
    // projection. The existing compactor can use these exact pins as-is.
    let Some(committed) = sess
        .clone_history()
        .await
        .estimate_token_count(turn_context)
    else {
        return false;
    };
    let committed_saved = sess
        .calibrated_prompt_tokens(turn_context, before)
        .await
        .saturating_sub(sess.calibrated_prompt_tokens(turn_context, committed).await);
    if status
        .auto_compact_scope_tokens
        .saturating_sub(committed_saved)
        >= limit
        || status
            .full_context_window_limit
            .is_some_and(|full| full_window_tokens.saturating_sub(committed_saved) >= full)
    {
        return false;
    }
    info!(
        retired_outputs = call_ids.len(),
        estimated_saved_tokens = saved,
        "retired consumed tool outputs instead of compacting"
    );
    true
}

/// Describes a compaction failure the turn recovers from. The overflow error's
/// own text tells the user to start a new thread, which is wrong while local
/// compaction or exact retirement continues the turn.
fn recovered_compaction_failure(error: &CodexErr) -> String {
    match error {
        CodexErr::ContextWindowExceeded => {
            "the compaction input exceeds the model's context window".to_string()
        }
        error => error.to_string().trim_end_matches('.').to_string(),
    }
}

/// A failed local-only summary gets the exact retirement a failed remote
/// summary already gets: only the newest generation stays inline, and the
/// turn continues only when that alone clears the pressure its caller measured.
async fn retire_after_failed_local_compaction(
    sess: &Session,
    turn_context: &TurnContext,
    retire_consumed_outputs: ConsumedOutputRetirement,
    local_error: CodexErr,
) -> CodexResult<ContextPressureRelief> {
    let ConsumedOutputRetirement::Eligible { projected_pressure } = retire_consumed_outputs else {
        return Err(local_error);
    };
    if matches!(local_error, CodexErr::Interrupted | CodexErr::TurnAborted)
        || !retire_consumed_tool_outputs_for_context_pressure(
            sess,
            turn_context,
            FAILED_COMPACTION_KEPT_GENERATIONS,
            projected_pressure,
        )
        .await
    {
        return Err(local_error);
    }
    sess.send_event(
        turn_context,
        EventMsg::Warning(WarningEvent {
            message: format!(
                "Local compaction failed: {}. Retired consumed tool results to recovery pins instead; the turn continues.",
                recovered_compaction_failure(&local_error)
            ),
        }),
    )
    .await;
    Ok(ContextPressureRelief::Compaction)
}

#[instrument(
    level = "trace",
    skip_all,
    fields(reason = ?reason, phase = ?phase)
)]
#[allow(clippy::too_many_arguments)]
fn run_auto_compact<'a>(
    sess: &'a Arc<Session>,
    step_context: Arc<StepContext>,
    fallback_step_context: Option<Arc<StepContext>>,
    client_session: &'a mut ModelClientSession,
    prefetched_workspace_identity: Option<
        &'a Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    >,
    prefetched_world_state: Option<Arc<WorldState>>,
    reason: CompactionReason,
    phase: CompactionPhase,
    retire_consumed_outputs: ConsumedOutputRetirement,
    cancellation_token: &'a CancellationToken,
) -> BoxFuture<'a, CodexResult<ContextPressureRelief>> {
    // Bound the compaction future at the shared dispatch boundary. Otherwise
    // each pre-sampling/planning frame embeds every compaction implementation.
    Box::pin(async move {
        let turn_context = &step_context.turn;
        let budget_turn_context = Arc::clone(turn_context);
        // Each caller reports a returned failure once, at its terminal boundary.
        // Emit no Error event here: a remote failure may still be recovered
        // below, and inner paths report only some failures inline.
        // Consumed results already have exact recovery pins. Retiring them
        // costs no model request; summarize only when that cannot bring the
        // window under its limit.
        if let ConsumedOutputRetirement::Eligible { projected_pressure } = retire_consumed_outputs
            && retire_consumed_tool_outputs_for_context_pressure(
                sess,
                turn_context,
                CONTEXT_PRESSURE_KEPT_GENERATIONS,
                projected_pressure,
            )
            .await
        {
            return Ok(ContextPressureRelief::RoutineRetirement);
        }
        // Automatic compaction restores initial context. Reuse a snapshot already
        // captured by the caller, or capture one before dispatching compaction.
        let world_state = match prefetched_world_state {
            Some(world_state) => world_state,
            None => Arc::new(sess.build_world_state_for_step(step_context.as_ref()).await),
        };
        let initial_context_injection = InitialContextInjection::AtStart(world_state);
        let remote_compaction = should_use_remote_compact_task(
            turn_context.provider.info(),
            turn_context.config.compact_prompt.as_deref(),
        );
        let mut compact_locally = !remote_compaction;
        if remote_compaction {
            emit_compact_metric(
                &sess.services.session_telemetry,
                "remote_v2",
                /*manual*/ false,
            );
            let remote_result = run_inline_remote_auto_compact_task_v2(
                Arc::clone(sess),
                Arc::clone(&step_context),
                fallback_step_context.clone(),
                client_session,
                initial_context_injection.clone(),
                reason,
                phase,
                /*emit_error_event*/ false,
                cancellation_token,
            )
            .await;
            if let Err(remote_error) = remote_result {
                if matches!(remote_error, CodexErr::TurnAborted) {
                    return Err(remote_error);
                }
                // A failed summary must not end a long turn: exact retirement
                // or a streamed local summary can still continue it.
                if let ConsumedOutputRetirement::Eligible { projected_pressure } =
                    retire_consumed_outputs
                    && retire_consumed_tool_outputs_for_context_pressure(
                        sess,
                        turn_context,
                        FAILED_COMPACTION_KEPT_GENERATIONS,
                        projected_pressure,
                    )
                    .await
                {
                    sess.send_event(
                        turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "Remote compaction failed: {}. Retired consumed tool results to recovery pins instead; the turn continues.",
                                recovered_compaction_failure(&remote_error)
                            ),
                        }),
                    )
                    .await;
                    return Ok(ContextPressureRelief::Compaction);
                }
                if !crate::compact_model_fallback::should_retry_with_local_compaction(&remote_error)
                {
                    return Err(remote_error);
                }
                if let Some(advice) = remote_error.retry_after() {
                    let _retry_timing_guard = turn_context.turn_timing_state.begin_retry_backoff();
                    crate::responses_retry::wait_for_retry_deadline(
                        advice.deadline(),
                        cancellation_token,
                    )
                    .await?;
                }
                sess.send_event(
                    turn_context,
                    EventMsg::Warning(WarningEvent {
                        message: format!(
                            "Remote compaction failed: {}. Compacting locally instead.",
                            recovered_compaction_failure(&remote_error)
                        ),
                    }),
                )
                .await;
                compact_locally = true;
            }
        }
        if compact_locally {
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
                /*emit_error_event*/ false,
                cancellation_token,
            )
            .await;

            match initial_attempt {
                Ok(()) => {}
                Err(previous_error) => {
                    let Some(fallback_step_context) = fallback_step_context else {
                        // A remote failure already tried this retirement above.
                        if remote_compaction {
                            return Err(previous_error);
                        }
                        return retire_after_failed_local_compaction(
                            sess,
                            turn_context,
                            retire_consumed_outputs,
                            previous_error,
                        )
                        .await;
                    };
                    if !should_retry_with_current_model(&previous_error) {
                        return Err(previous_error);
                    }
                    if let Some(advice) = previous_error.retry_after() {
                        let _retry_timing_guard =
                            turn_context.turn_timing_state.begin_retry_backoff();
                        crate::responses_retry::wait_for_retry_deadline(
                            advice.deadline(),
                            cancellation_token,
                        )
                        .await?;
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
                        /*emit_error_event*/ false,
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
        if matches!(reason, CompactionReason::ContextLimit) {
            let token_status = super::context_window::context_window_token_status(
                sess,
                budget_turn_context.as_ref(),
            )
            .await;
            if token_status.token_limit_reached {
                return Err(CodexErr::Fatal(
                    "Compaction did not bring the context below its configured token limit. Stopped automatic continuation to prevent repeated compaction and usage drain; reduce context or start a new task.".to_string(),
                ));
            }
        }
        Ok(ContextPressureRelief::Compaction)
    })
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
            ResponseItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content_item| match content_item {
            ContentItem::InputText { text } => Some(text.clone()),
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
        deferred_dynamic_history: None,
        prompt_provenance: Default::default(),
        digests: PromptDigests::default(),
        tools: router.model_visible_schemas_for_turn(turn_context),
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        tool_calls_disabled: false,
        base_instructions,
        output_schema: turn_context
            .final_output_json_schema
            .as_ref()
            .map(|schema| schema.original().clone()),
        output_schema_strict: true,
        stream_progress_timeout: None,
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
                crate::context::desktop_instructions::project(desktop_context).into_owned(),
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
/// router would hand back. Keeping a weak reference reserves the allocation identity after
/// the router drops, so a later router cannot reuse its address and match a stale token.
#[derive(Clone, Debug)]
struct ToolSchemaSurfaceToken {
    router: Weak<ToolRouter>,
    activation_revision: u64,
    terminal_completion_only: bool,
}

impl ToolSchemaSurfaceToken {
    fn capture(
        router: &Arc<ToolRouter>,
        turn: &crate::session::turn_context::TurnContext,
        terminal_completion_only: bool,
    ) -> Self {
        Self {
            router: Arc::downgrade(router),
            activation_revision: turn.deferred_tool_activation_revision(),
            terminal_completion_only,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.router, &other.router)
            && self.activation_revision == other.activation_revision
            && self.terminal_completion_only == other.terminal_completion_only
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
        router: &Arc<ToolRouter>,
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
            && entry.tool_schema_surface.matches(&tool_schema_surface)
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

        let tools = router.model_visible_schemas_for_turn(&step_context.turn);
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
    build_projected_prompt_from_scaffold(prepared, step_context, &scaffold)
}

fn build_projected_prompt_from_scaffold(
    prepared: &PreparedPromptInput,
    step_context: &StepContext,
    resolved_scaffold: &ResolvedRequestScaffold,
) -> Prompt {
    let scaffold = resolved_scaffold.scaffold.as_ref();
    let [
        input,
        fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
    ] = prepared.shared_prompt_projections();
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
        tool_calls_disabled: false,
        base_instructions: scaffold.base_instructions.clone(),
        output_schema: step_context
            .turn
            .final_output_json_schema
            .as_ref()
            .map(|schema| schema.original().clone()),
        output_schema_strict: true,
        stream_progress_timeout: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sampling_request<'a>(
    sess: Arc<Session>,
    mut step_context: Arc<StepContext>,
    turn_store: Arc<codex_extension_api::ExtensionData>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &'a mut ModelClientSession,
    responses_metadata: &'a CodexResponsesMetadata,
    prepare_input: impl std::future::Future<Output = CodexResult<PreparedPromptInput>> + 'a,
    selected_skill_invocations: &'a [SkillInvocation],
    prebuilt_router: &'a mut Option<Arc<ToolRouter>>,
    base_instructions: &'a BaseInstructions,
    preparation_timing_guard: &'a mut Option<TurnTimingGuard>,
    generation_request: GenerationRequestDisposition,
    generation_id: ModelGenerationId,
    request_signals: SamplingRequestSignalCollector,
    pending_continuation_cause: &'a mut Option<ContinuationCause>,
    cancellation_token: CancellationToken,
) -> impl std::future::Future<Output = CodexResult<(SamplingRequestResult, Arc<[ResponseItem]>)>> + 'a
{
    Box::pin(async move {
        let turn_context = Arc::clone(&step_context.turn);
        let terminal_completion_only = generation_request.terminal_completion_only;
        // Record the deferred schemas advertised by this request. Settling the guard preserves them
        // for later continuations in the same turn; capability refresh and turn teardown own expiry.
        let _advertised_deferred_tool_lease = (!terminal_completion_only).then(|| {
            let advertised_deferred_tools = turn_context.activated_deferred_tools();
            AdvertisedDeferredToolLease::new(Arc::clone(&turn_context), advertised_deferred_tools)
        });
        let router_preparation_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::RouterBuild);
        let cached_router = prebuilt_router.take();
        let mut router = match cached_router {
            Some(router) if terminal_completion_only => {
                // Preserve the last schema surface for the terminal answer.
                // Dispatch still rejects any invalid model-emitted tool call.
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
                    &step_context,
                    selected_skill_invocations,
                    &cancellation_token,
                )
                .await?
            }
            None => {
                built_tools(
                    sess.as_ref(),
                    &step_context,
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
        drop(router_preparation_guard);
        record_context_notice_if_changed(
            sess.as_ref(),
            turn_context.as_ref(),
            "tool_search_sources",
            router.tool_search_sources(),
        )
        .await?;
        let prepared_input = prepare_input.await?;
        let scaffold_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::RequestTransformation);
        let mut request_scaffold = sess
            .request_scaffold_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resolve(
                &prepared_input,
                sess.as_ref(),
                &router,
                step_context.as_ref(),
                base_instructions,
                terminal_completion_only,
            );
        let pending_tool_manifest = Arc::new(Mutex::new({
            let previous_manifest_hash = sess
                .queued_tool_manifest_hash
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let manifest = router.tool_manifest_for_rollout(
                turn_context.as_ref(),
                previous_manifest_hash.as_deref(),
            );
            Some(manifest)
        }));

        drop(scaffold_guard);
        let worker_preparation_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Preparation);
        let mut tool_runtime = ToolCallRuntime::new(
            Arc::clone(&sess),
            Arc::clone(&step_context),
            Arc::clone(&turn_diff_tracker),
        )
        .with_sampling_request_signals(request_signals.clone())
        .with_terminal_completion_only(terminal_completion_only);
        let mut _code_mode_worker = (!terminal_completion_only).then(|| {
            sess.services.code_mode_service.start_turn_worker(
                &sess,
                Arc::clone(&step_context),
                Arc::clone(&turn_diff_tracker),
                request_signals.clone(),
            )
        });
        drop(worker_preparation_guard);
        let max_retries = turn_context.provider.info().stream_max_retries();
        let mut retry_state = ResponsesStreamRetryState::default();
        let recovery_episode_id = format!("{}:{}", generation_id.turn_id, generation_id.ordinal);
        let mut outer_attempt_ordinal = 0_u64;
        let mut outer_retry_reason = "initial";
        let mut account_wait_deadline = None;
        let mut accepted_attempt_input = prepared_input.shared_items();
        sess.services
            .code_mode_service
            .note_visible_prompt(Arc::clone(&accepted_attempt_input));
        let prompt_construction_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::PromptConstruction);
        let mut prompt = build_projected_prompt_from_scaffold(
            &prepared_input,
            step_context.as_ref(),
            &request_scaffold,
        );
        enforce_terminal_prompt_contract(&mut prompt, terminal_completion_only);
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
            // During a logout or account switch a dispatched request carries no
            // credentials and is rejected; wait for the login instead.
            if turn_context.provider.missing_required_credentials()
                && wait_for_account_change(
                    AccountChangeWait::MissingCredentials,
                    &mut account_wait_deadline,
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &cancellation_token,
                )
                .await?
            {
                continue;
            }
            let mut attempt_progress = SamplingAttemptProgress::default();
            // The streaming/tool state machine is large in debug builds. Keep its
            // storage off the enclosing turn future without changing poll ownership.
            let err = match Box::pin(try_run_sampling_request(
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
                Arc::clone(&pending_tool_manifest),
                cancellation_token.child_token(),
                &mut attempt_progress,
            ))
            .instrument(trace_span!("try_run_sampling_request",
                turn_id = %turn_context.sub_id,
                recovery_episode_id = %recovery_episode_id,
                outer_attempt_ordinal,
                outer_retry_reason,
                model = %turn_context.model_info.slug))
            .await
            {
                Ok(output) => {
                    return Ok((output, accepted_attempt_input));
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
                    // An account switch or limit reset makes the same request
                    // succeed; it does not spend the stream retry budget.
                    if !wait_for_account_change(
                        AccountChangeWait::UsageLimit(&e, client_session.dispatched_account()),
                        &mut account_wait_deadline,
                        sess.as_ref(),
                        turn_context.as_ref(),
                        &cancellation_token,
                    )
                    .await?
                    {
                        return Err(CodexErr::UsageLimitReached(e));
                    }
                    None
                }
                Err(err) => Some(err),
            };

            if let Some(err) = err {
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
            }
            turn_context.turn_timing_state.record_sampling_retry();
            outer_attempt_ordinal = outer_attempt_ordinal.saturating_add(1);
            outer_retry_reason = "response_stream_or_account_recovery";
            if !attempt_progress.requires_authoritative_retry_input() {
                // Nothing was accepted, so the next dispatch replays the same logical
                // request rather than continuing the turn.
                client_session.note_logical_request_replay();
            }
            if attempt_progress.requires_authoritative_retry_input() {
                // Accepted output has changed authoritative state. Revalidate the
                // whole request snapshot while retaining this recovery episode's budget.
                let authoring_epoch = step_context.user_intent_epoch;
                step_context = sess.capture_step_context(Arc::clone(&turn_context)).await?;
                Arc::make_mut(&mut step_context).user_intent_epoch = authoring_epoch;
                if !terminal_completion_only
                    && !finalized_router_matches_current_exposure(
                        sess.as_ref(),
                        step_context.as_ref(),
                        router.as_ref(),
                    )
                    .await
                {
                    router = built_tools(
                        sess.as_ref(),
                        &step_context,
                        selected_skill_invocations,
                        &cancellation_token,
                    )
                    .await?;
                }
                step_context
                    .set_tool_router(Arc::clone(&router))
                    .map_err(|_| {
                        CodexErr::Fatal("recovery step router was already finalized".to_string())
                    })?;
                *prebuilt_router = Some(Arc::clone(&router));
                record_context_notice_if_changed(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    "tool_search_sources",
                    router.tool_search_sources(),
                )
                .await?;
                let retry_input = prepare_sampling_prompt_for_client(
                    sess.clone_history().await,
                    turn_context.as_ref(),
                    sess.services.git_workspace.as_ref(),
                )
                .await;
                request_scaffold = sess
                    .request_scaffold_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .resolve(
                        &retry_input,
                        sess.as_ref(),
                        &router,
                        step_context.as_ref(),
                        base_instructions,
                        terminal_completion_only,
                    );
                *pending_tool_manifest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = {
                    let previous = sess
                        .queued_tool_manifest_hash
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    Some(
                        router
                            .tool_manifest_for_rollout(turn_context.as_ref(), previous.as_deref()),
                    )
                };
                accepted_attempt_input = retry_input.shared_items();
                sess.services
                    .code_mode_service
                    .note_visible_prompt(Arc::clone(&accepted_attempt_input));
                prompt = build_projected_prompt_from_scaffold(
                    &retry_input,
                    step_context.as_ref(),
                    &request_scaffold,
                );
                enforce_terminal_prompt_contract(&mut prompt, terminal_completion_only);
            } else {
                // Even a stream with no completed output seals its evidence batch
                // during cleanup. Reusing the prompt must not reuse that retired batch.
                step_context = Arc::new(step_context.for_retry_attempt());
            }
            tool_runtime = ToolCallRuntime::new(
                Arc::clone(&sess),
                Arc::clone(&step_context),
                Arc::clone(&turn_diff_tracker),
            )
            .with_sampling_request_signals(request_signals.clone())
            .with_terminal_completion_only(terminal_completion_only);
            _code_mode_worker = (!terminal_completion_only).then(|| {
                sess.services.code_mode_service.start_turn_worker(
                    &sess,
                    Arc::clone(&step_context),
                    Arc::clone(&turn_diff_tracker),
                    request_signals.clone(),
                )
            });
        }
    })
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
    // Dispatch also rejects unexpected calls; do not rewrite the cached schema
    // prefix or parallel-call setting just to request a final answer.
    prompt.tool_calls_disabled = terminal_completion_only;
}

async fn finalized_router_matches_current_exposure(
    sess: &Session,
    step_context: &StepContext,
    router: &ToolRouter,
) -> bool {
    if !router.matches_planning_context(step_context.turn.as_ref()) {
        return false;
    }
    if router.exposure_identity().dynamic_identity()
        != current_dynamic_tool_exposure_identity(sess, step_context).await
    {
        return false;
    }
    let turn_context = step_context.turn.as_ref();
    if !tool_suggest_enabled(turn_context) {
        return router.tool_suggest_candidates.is_none();
    }
    let loaded_plugins = sess
        .services
        .plugins_manager
        .plugins_for_config(&turn_context.config.plugins_config_input())
        .await;
    // Advisory catalog fetches can finish without changing the turn config.
    router.tool_suggest_candidates
        == cached_tool_suggest_candidates(sess, turn_context, &loaded_plugins).await
}

async fn cached_tool_suggest_candidates(
    sess: &Session,
    turn_context: &TurnContext,
    loaded_plugins: &PluginLoadOutcome,
) -> Option<ToolSuggestCandidates> {
    if !tool_suggest_enabled(turn_context) {
        return None;
    }
    let auth = sess.services.auth_manager.auth().await;
    let plugins_config = turn_context.config.plugins_config_input();
    sess.services
        .plugins_manager
        .cached_recommended_plugin_candidates_for_config(RecommendedPluginCandidatesInput {
            plugins_config: &plugins_config,
            loaded_plugins,
            auth: auth.as_ref(),
            disabled_tools: &turn_context.config.tool_suggest.disabled_tools,
            app_server_client_name: turn_context.app_server_client_name.as_deref(),
        })
        .map(|tools| ToolSuggestCandidates {
            tools,
            presentation: ToolSuggestPresentation::RecommendationContext,
        })
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
        native_environment_contract: crate::tools::spec_plan::native_environment_contract(
            step_context,
        ),
        windows_shell_guidance: crate::tools::spec_plan::requires_windows_shell_guidance(
            step_context,
        ),
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
                    && entry.router.matches_planning_context(turn_context)
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
    step_context: &Arc<StepContext>,
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
        && prepared.router.matches_planning_context(turn_context)
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
    step_context: &Arc<StepContext>,
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
    let extension_snapshot = extension_tool_surface_snapshot(sess);
    let extension_tool_executors = extension_snapshot.tools;
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
        extension_snapshot.revision,
    );
    let connector_snapshot = step_context.mcp.config().connector_snapshot.clone();

    let apps_enabled = turn_context.apps_enabled();
    let accessible_connectors =
        apps_enabled.then(|| connectors::accessible_connectors_from_mcp_tools(all_mcp_tools));
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
    let tool_suggest_candidates =
        cached_tool_suggest_candidates(sess, turn_context, &loaded_plugins).await;
    let mcp_tool_exposure = build_mcp_tool_exposure(
        all_mcp_tools,
        connectors.as_deref(),
        &turn_context.config,
        search_tool_enabled(turn_context),
        &selected_skill_mcp_exposure.selection,
    );
    let mcp_tools = has_mcp_servers.then_some(mcp_tool_exposure.direct_tools);
    let deferred_mcp_tools = mcp_tool_exposure.deferred_tools;
    // Index construction and concurrent cache-build waits can block. Keep them
    // off runtime workers so cancellation and other turns can still progress.
    let router_step = Arc::clone(step_context);
    let search_cache = Arc::clone(&sess.services.tool_search_handler_cache);
    let span = tracing::Span::current();
    let router_build = AbortOnDropHandle::new(tokio::task::spawn_blocking(move || {
        let _enter = span.enter();
        ToolRouter::try_from_context(
            router_step.as_ref(),
            ToolRouterParams {
                mcp_tools,
                deferred_mcp_tools,
                tool_suggest_candidates,
                extension_tool_executors,
                dynamic_tools: router_step.turn.dynamic_tools.as_slice(),
                exposure_identity,
            },
            &search_cache,
        )
    }));
    let router = Arc::new(
        router_build
            .or_cancel(cancellation_token)
            .await?
            .map_err(|error| CodexErr::Fatal(format!("tool router build task failed: {error}")))?
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
    extension_tool_surface_revision: u64,
) -> ToolExposureIdentity {
    let turn_context = step_context.turn.as_ref();
    let agent_surface_stage = agent_surface_stage(sess, turn_context);
    let goal_surface_state = goal_surface_state(extension_tool_executors);
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
        native_environment_contract: crate::tools::spec_plan::native_environment_contract(
            step_context,
        ),
        windows_shell_guidance: crate::tools::spec_plan::requires_windows_shell_guidance(
            step_context,
        ),
    }
}

fn request_user_input_eligible(turn_context: &TurnContext) -> bool {
    let available_modes = request_user_input_available_modes(turn_context.config.features.get());
    turn_context.config.experimental_request_user_input_enabled
        && !turn_context.session_source.is_non_root_agent()
        && available_modes.contains(&turn_context.collaboration_mode.mode)
}

pub(super) fn agent_surface_stage(sess: &Session, turn_context: &TurnContext) -> AgentSurfaceStage {
    let spawn_eligible = match turn_context.multi_agent_version {
        MultiAgentVersion::Disabled => false,
        MultiAgentVersion::V1 => !crate::agent::exceeds_thread_spawn_depth_limit(
            crate::agent::next_thread_spawn_depth(&turn_context.session_source),
            turn_context.config.agent_max_depth,
        ),
        MultiAgentVersion::V2 => true,
    };
    let control = &sess.services.agent_control;
    agent_surface_stage_from_snapshot(
        spawn_eligible,
        // Latched rather than current: a graph that empties when children finish
        // must not retract tool schemas that sit in the cached prompt prefix.
        control.ever_had_live_agents(),
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

const MISSING_ANSWER_REPAIR_DIRECTIVE: &str = "The response ended without a completed final answer. Deliver the answer required by the current request, including any requested explanation or truthful blocker; Commentary and successful tool stdout are not a final answer. Do not repeat completed effects or invent validation. If and only if the request explicitly authorizes no prose, and no structured output schema applies, declare that disposition with an empty final_answer message rather than an empty response. Otherwise supply a nonempty final answer. This is the single missing-answer repair attempt.";

#[derive(Debug, Default)]
enum AnswerDelivery {
    #[default]
    Missing,
    Message(codex_protocol::protocol::AcceptedFinalMessage),
    /// An explicit completed final item, not a reasoning-only/empty response.
    NoProse,
}

#[derive(Debug, Eq, PartialEq)]
enum VisibleOutputNovelty {
    Unobserved,
    NewCommunication,
    ExactRepeat,
    Replay,
}

/// Turn-local only: one answer and one visible identity. Observation is not
/// permission to suppress output, skip reasoning, or accept unfinished work.
#[derive(Default)]
struct TurnOutputState {
    revision: Option<String>,
    answer: AnswerDelivery,
    last_visible: Option<(String, Option<MessagePhase>, [u8; 32])>,
    missing_answer_repair_attempted: bool,
}

impl TurnOutputState {
    fn reconcile_revision(&mut self, revision: &str) {
        if self.revision.as_deref() != Some(revision) {
            self.invalidate_answer();
            self.last_visible = None;
            self.revision = Some(revision.to_string());
        }
    }

    fn invalidate_answer(&mut self) {
        self.answer = AnswerDelivery::Missing;
    }

    fn invalidate_for_repair(&mut self) {
        self.invalidate_answer();
        self.last_visible = None;
        self.revision = None;
    }

    fn observe(
        &mut self,
        output: Option<FinalizedAssistantOutput>,
        candidate: Option<codex_protocol::protocol::AcceptedFinalMessage>,
        revision: String,
        allow_answer: bool,
        allow_no_prose: bool,
    ) -> VisibleOutputNovelty {
        use sha2::Digest;
        self.reconcile_revision(&revision);
        if !allow_answer {
            // Output accompanying tools may precede their effects/results.
            self.invalidate_answer();
        } else if let Some(candidate) = candidate {
            self.answer = AnswerDelivery::Message(candidate);
        } else if allow_no_prose
            && self.missing_answer_repair_attempted
            && output.as_ref().is_some_and(|output| {
                output.phase == Some(MessagePhase::FinalAnswer) && output.text.is_empty()
            })
        {
            self.answer = AnswerDelivery::NoProse;
        }
        let Some(output) = output else {
            // Necessary silent inference has no observable novelty verdict.
            return VisibleOutputNovelty::Unobserved;
        };
        let digest: [u8; 32] = sha2::Sha256::digest(output.text.as_bytes()).into();
        let novelty = match &self.last_visible {
            Some((id, phase, previous)) if phase == &output.phase && previous == &digest => {
                if id == &output.item_id {
                    VisibleOutputNovelty::Replay
                } else {
                    VisibleOutputNovelty::ExactRepeat
                }
            }
            _ => VisibleOutputNovelty::NewCommunication,
        };
        self.last_visible = Some((output.item_id, output.phase, digest));
        tracing::debug!(
            ?novelty,
            "task-bound visible output observation; semantic necessity unknown"
        );
        novelty
    }

    fn has_answer(&self) -> bool {
        !matches!(self.answer, AnswerDelivery::Missing)
    }

    fn answer_text(&self) -> Option<String> {
        match &self.answer {
            AnswerDelivery::Message(message) => Some(message.text.clone()),
            _ => None,
        }
    }

    fn take_answer(&mut self) -> Option<codex_protocol::protocol::AcceptedFinalMessage> {
        match std::mem::take(&mut self.answer) {
            AnswerDelivery::Message(message) => Some(message),
            _ => None,
        }
    }
}

fn final_message_candidate_from_output(
    output: crate::stream_events_utils::FinalizedAssistantOutput,
) -> Option<codex_protocol::protocol::AcceptedFinalMessage> {
    use codex_protocol::models::MessagePhase;
    use codex_protocol::protocol::AcceptedFinalMessageOrigin;
    if output.phase == Some(MessagePhase::Commentary) || output.text.trim().is_empty() {
        return None;
    }
    Some(codex_protocol::protocol::AcceptedFinalMessage {
        item_id: output.item_id,
        text: output.text,
        origin: if output.phase == Some(MessagePhase::FinalAnswer) {
            AcceptedFinalMessageOrigin::AssistantFinal
        } else {
            AcceptedFinalMessageOrigin::LegacyUnphased
        },
        completion_attempt_id: String::new(),
    })
}

fn update_final_message_candidate(
    current: &mut Option<codex_protocol::protocol::AcceptedFinalMessage>,
    candidate: codex_protocol::protocol::AcceptedFinalMessage,
) {
    use codex_protocol::protocol::AcceptedFinalMessageOrigin;
    let rank = |origin| match origin {
        AcceptedFinalMessageOrigin::AssistantFinal => 2,
        AcceptedFinalMessageOrigin::LegacyUnphased => 1,
        AcceptedFinalMessageOrigin::Plan => 0,
    };
    if current
        .as_ref()
        .is_none_or(|previous| rank(candidate.origin) >= rank(previous.origin))
    {
        *current = Some(candidate);
    }
}

#[derive(Debug)]
struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
    assistant_output: Option<FinalizedAssistantOutput>,
    final_message_candidate: Option<codex_protocol::protocol::AcceptedFinalMessage>,
    settled_state: SamplingRequestSettledState,
    tool_result_continuation: bool,
    server_end_turn_false: bool,
    required_tool_terminal: Option<RequiredToolTerminal>,
    prefetched_workspace_identity: Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    continuation_workspace_prefetch: Option<ContinuationWorkspacePrefetch>,
}

#[derive(Debug)]
struct UnsettledSamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
    assistant_output: Option<FinalizedAssistantOutput>,
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
    /// Text already delivered to clients, retained if the final item never arrives.
    streamed_text: String,
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
struct PlanModeStreamState {
    /// Agent message items started by the model but deferred until we see non-plan text.
    pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: BTreeMap<String, TurnItem>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: BTreeMap::new(),
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
        if !self.parsers_by_item.contains_key(item_id) {
            self.parsers_by_item.insert(
                item_id.to_owned(),
                AssistantTextStreamParser::new(self.plan_mode),
            );
        }
        self.parsers_by_item
            .get_mut(item_id)
            .unwrap_or_else(|| unreachable!("stream parser was inserted above"))
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
            streamed_text: String::new(),
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
        // A delta without a preceding ItemStarted event produces an invalid
        // client-visible item stream. Treat both out-of-phase cases as inert.
        if !self.started || self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        self.streamed_text.push_str(delta);
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
#[cfg(test)]
type PlanStartPublicationGate = (
    async_channel::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static PLAN_START_PUBLICATION_GATES: OnceLock<
    Mutex<HashMap<codex_protocol::ThreadId, PlanStartPublicationGate>>,
> = OnceLock::new();

async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains_key(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        #[cfg(test)]
        {
            let gate = PLAN_START_PUBLICATION_GATES
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .remove(&sess.thread_id);
            if let Some((reached, release)) = gate {
                let _ = reached.send(()).await;
                let _ = release.await;
            }
        }
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string(), item);
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

// A first prose batch can await ItemStarted publication after extracting buffered
// whitespace. Retain that accepted batch across sampling-worker abort, including
// the interval where its returned state is queued but the caller has not resumed.
struct OwnedPlanStreamState {
    state: Option<PlanModeStreamState>,
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    in_flight: Option<super::DurableHistoryCommitInFlight>,
    runtime: tokio::runtime::Handle,
}

impl Drop for OwnedPlanStreamState {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        let turn = Arc::clone(&self.turn);
        let in_flight = self.in_flight.take();
        self.runtime.spawn(async move {
            let _in_flight = in_flight;
            complete_partial_plan_items(&session, &turn, &mut state).await;
        });
    }
}

async fn complete_partial_plan_items(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
) {
    for (_, item) in std::mem::take(&mut state.started_agent_message_items) {
        sess.emit_turn_item_completed(turn_context, item).await;
    }
    let plan_text = std::mem::take(&mut state.plan_item_state.streamed_text);
    state
        .plan_item_state
        .complete_with_text(sess, turn_context, plan_text)
        .await;
}

#[expect(
    clippy::expect_used,
    reason = "Owned state exists until its single handoff; a failed publication task cannot supply a replacement state"
)]
async fn handle_plan_segments(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    let starts_prose = !state.started_agent_message_items.contains_key(item_id)
        && segments.iter().any(|segment| {
            matches!(segment,
            ProposedPlanSegment::Normal(delta) if delta.chars().any(|ch| !ch.is_whitespace()))
        });
    if !starts_prose {
        handle_plan_segments_inner(sess, turn_context, state, item_id, segments).await;
        return;
    }
    sess.durable_history_commits_in_flight
        .fetch_add(1, Ordering::AcqRel);
    let mut owned = OwnedPlanStreamState {
        state: Some(std::mem::replace(
            state,
            PlanModeStreamState::new(&turn_context.sub_id),
        )),
        session: Arc::clone(sess),
        turn: Arc::clone(turn_context),
        in_flight: Some(super::DurableHistoryCommitInFlight {
            session: Arc::clone(sess),
        }),
        runtime: tokio::runtime::Handle::current(),
    };
    let session = Arc::clone(sess);
    let turn = Arc::clone(turn_context);
    let item_id = item_id.to_string();
    let (send_state, receive_state) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        handle_plan_segments_inner(
            &session,
            &turn,
            owned.state.as_mut().expect("owned plan state"),
            &item_id,
            segments,
        )
        .await;
        // On an abandoned receiver, Drop closes the actual published prefix.
        let _ = send_state.send(owned);
    });
    let mut returned = receive_state
        .await
        .expect("owned plan publication task must return its state");
    *state = returned.state.take().expect("returned plan state");
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments_inner(
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
                if !has_non_whitespace && !state.started_agent_message_items.contains_key(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains_key(item_id) {
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
                if let Some(item) = state.started_agent_message_items.get_mut(item_id) {
                    apply_partial_agent_message_delta(item, &event);
                }
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

fn append_partial_reasoning_text(parts: &mut Vec<String>, index: i64, delta: &str) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    if let Some(part) = parts.get_mut(index) {
        part.push_str(delta);
    } else if index == parts.len() {
        parts.push(delta.to_string());
    }
}

fn apply_partial_agent_message_delta(item: &mut TurnItem, event: &AgentMessageContentDeltaEvent) {
    let TurnItem::AgentMessage(message) = item else {
        return;
    };
    if !event.delta.is_empty() {
        match message.content.last_mut() {
            Some(codex_protocol::items::AgentMessageContent::Text { text }) => {
                text.push_str(&event.delta);
            }
            None => message
                .content
                .push(codex_protocol::items::AgentMessageContent::Text {
                    text: event.delta.clone(),
                }),
        }
    }
}

async fn emit_streamed_assistant_text_delta(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    item_id: &str,
    parsed: ParsedAssistantTextDelta,
) -> Option<AgentMessageContentDeltaEvent> {
    if parsed.is_empty() {
        return None;
    }
    if let Some(state) = plan_mode_state {
        if !parsed.plan_segments.is_empty() {
            handle_plan_segments(sess, turn_context, state, item_id, parsed.plan_segments).await;
        }
        return None;
    }
    if parsed.visible_text.is_empty() {
        return None;
    }
    let event = AgentMessageContentDeltaEvent {
        thread_id: sess.thread_id.to_string(),
        turn_id: turn_context.sub_id.clone(),
        item_id: item_id.to_string(),
        delta: parsed.visible_text,
    };
    sess.send_event(
        turn_context,
        EventMsg::AgentMessageContentDelta(event.clone()),
    )
    .await;
    Some(event)
}

/// Flush buffered assistant text parser state when an assistant message item ends.
async fn flush_assistant_text_segments_for_item(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
    item_id: &str,
) {
    let parsed = parsers.finish_item(item_id);
    let _ =
        emit_streamed_assistant_text_delta(sess, turn_context, plan_mode_state, item_id, parsed)
            .await;
}

/// Flush any remaining buffered assistant text parser state at response completion.
async fn flush_assistant_text_segments_all(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    mut plan_mode_state: Option<&mut PlanModeStreamState>,
    parsers: &mut AssistantMessageStreamParsers,
) -> Vec<AgentMessageContentDeltaEvent> {
    let mut emitted = Vec::new();
    for (item_id, parsed) in parsers.drain_finished() {
        if let Some(event) = emit_streamed_assistant_text_delta(
            sess,
            turn_context,
            plan_mode_state.as_deref_mut(),
            &item_id,
            parsed,
        )
        .await
        {
            emitted.push(event);
        }
    }
    emitted
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
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
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
        if let Some(started_item) = state.started_agent_message_items.remove(&agent_message_id) {
            sess.emit_turn_item_completed(turn_context, started_item)
                .await;
        }
        return;
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains_key(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                    phase: None,
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone(), start_item);
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
#[allow(clippy::too_many_arguments)]
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_store: &codex_extension_api::ExtensionData,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
    assistant_output: &mut Option<FinalizedAssistantOutput>,
    final_message_candidate: &mut Option<codex_protocol::protocol::AcceptedFinalMessage>,
) -> std::io::Result<bool> {
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
        if let Some(candidate) = finalized_facts
            .as_ref()
            .and_then(|facts| facts.assistant_output.clone())
            .and_then(final_message_candidate_from_output)
        {
            update_final_message_candidate(final_message_candidate, candidate);
        } else if final_message_candidate.is_none()
            && let ResponseItem::Message { content, .. } = item
        {
            let text = content
                .iter()
                .filter_map(|entry| match entry {
                    ContentItem::OutputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if let Some(text) = extract_proposed_plan_text(&text) {
                *final_message_candidate = Some(codex_protocol::protocol::AcceptedFinalMessage {
                    item_id: state.plan_item_state.item_id.clone(),
                    text,
                    origin: codex_protocol::protocol::AcceptedFinalMessageOrigin::Plan,
                    completion_attempt_id: String::new(),
                });
            }
        }

        record_completed_response_item_with_finalized_facts(
            sess,
            turn_context,
            item,
            finalized_facts.as_ref(),
        )
        .await?;
        if let Some(agent_message) = final_last_agent_message {
            *last_agent_message = Some(agent_message);
        }
        if let Some(output) = finalized_facts.and_then(|facts| facts.assistant_output) {
            *assistant_output = Some(output);
        }
        return Ok(true);
    }
    Ok(false)
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
        let (response_input, required_terminal, completion_error, failure_detail) =
            match stale_relay_error {
                Some(err) => {
                    let response_input = ToolCallRuntime::failure_response_for_message(
                        &completion.call,
                        err.to_string(),
                    );
                    (response_input, None, Some(err), None)
                }
                None => match completion.result {
                    Ok(completion) => (
                        completion.response,
                        completion.required_terminal,
                        None,
                        completion.failure_detail,
                    ),
                    Err(err) => {
                        error!("in-flight tool future failed during drain: {err}");
                        let response_input = ToolCallRuntime::failure_response_for_message(
                            &completion.call,
                            err.to_string(),
                        );
                        (response_input, None, Some(err), None)
                    }
                },
            };
        let mut response_items = vec![response_input.into()];
        response_items.extend(failure_detail);
        let commit = sess.record_tool_completion_ordered(
            &turn_context, &completion.call_id, &response_items,
        );
        tokio::pin!(commit);
        let commit_result = loop {
            tokio::select! {
                biased;
                result = &mut commit => break result,
                Some(result) = in_flight.next(), if !in_flight.is_empty() => {
                    // Keep admitted work progressing, but never deliver a
                    // successor before this durable ordered commit finishes.
                    completed.push_back(result);
                }
            }
        };
        if let Err(err) = commit_result {
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

pub(crate) async fn service_tier_for_sampling(
    sess: &Session,
    turn: &TurnContext,
) -> Option<String> {
    // Settings bursts may settle while planning or tools are running. Collapse
    // them at the request boundary instead of pinning every request to the
    // transient value captured when the turn was constructed.
    super::get_service_tier(
        sess.configured_service_tier().await,
        turn.config.features.enabled(Feature::FastMode),
        &turn.model_info,
    )
}

/// Token estimates for the largest transport representation of a prepared prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PromptTokenEstimates {
    /// The content-based local estimate, with no provider calibration.
    pub(crate) local_tokens: i64,
    /// The local estimate with an observed prompt prefix calibrated to its
    /// provider measurement; content past that prefix keeps its local cost.
    pub(crate) calibrated_tokens: i64,
}

/// Estimates immutable prepared inputs, including every eligible transport fallback.
/// No session lock is held while estimating content or preparing Lite images.
pub(crate) async fn prompt_token_estimates(
    sess: &Session,
    turn: &TurnContext,
    prompt: &Prompt,
) -> PromptTokenEstimates {
    let lite = turn.model_info.use_responses_lite;
    let inputs = prompt.get_formatted_admission_inputs(lite);
    let mut bases = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        if inputs[..index]
            .iter()
            .any(|previous| Arc::ptr_eq(previous, input))
        {
            continue;
        }
        bases.push(prompt.calibration_basis(Arc::clone(input)));
    }
    let local_tokens = bases
        .iter()
        .map(|basis| basis.local_tokens)
        .max()
        .unwrap_or(0);
    let calibrated_tokens = {
        let state = sess.state.lock().await;
        bases
            .iter()
            .map(|basis| {
                state.prompt_token_calibration.estimate_prompt(
                    &turn.config.model_provider_id,
                    &turn.model_info.slug,
                    basis,
                )
            })
            .max()
            .unwrap_or(0)
    };
    PromptTokenEstimates {
        local_tokens,
        calibrated_tokens,
    }
}

/// A prompt that sampling admission refused before dispatch, so the provider
/// never measured it. The projection uses local estimates: relief rewrites the
/// observed prefix, after which admission has no calibration for the prompt.
#[derive(Clone, Copy, Debug)]
struct AdmissionRefusedProjection(ProjectedPromptPressure);

/// Admit immutable prepared inputs, including every eligible transport fallback.
async fn admit_prepared_prompt(
    sess: &Session,
    turn: &TurnContext,
    prompt: &Prompt,
) -> CodexResult<()> {
    let PromptTokenEstimates {
        local_tokens,
        calibrated_tokens: total,
    } = prompt_token_estimates(sess, turn, prompt).await;
    let (scope, local_scope) = match turn.config.model_auto_compact_token_limit_scope {
        AutoCompactTokenLimitScope::Total => (total, local_tokens),
        // The window's first server-observed request input is its prefix. Until one is
        // observed, the admitted request establishes it: an estimated baseline covers
        // history only, so subtracting it from this full prepared request would charge
        // tool schemas, notices and pending input as body growth. Pre-sampling planning
        // checks pending input against that estimate, and the total-window ceiling below
        // still applies.
        AutoCompactTokenLimitScope::BodyAfterPrefix => {
            let server_prefill = sess
                .state
                .lock()
                .await
                .auto_compact_window_server_observed_prefill();
            (
                server_prefill.map_or(0, |prefill| total.saturating_sub(prefill)),
                server_prefill.map_or(0, |prefill| local_tokens.saturating_sub(prefill)),
            )
        }
    };
    let status =
        super::context_window::projected_context_window_token_status(sess, turn, total, scope)
            .await;
    // A terminal-only continuation retains its existing soft-limit deferral,
    // but never bypasses the effective total-window ceiling.
    if status.full_context_window_limit_reached
        || (status.token_limit_reached && !prompt.tool_calls_disabled)
    {
        // The turn relieves this projection instead of treating the refusal as
        // a provider overflow.
        turn.extension_data
            .insert(AdmissionRefusedProjection(ProjectedPromptPressure {
                total_tokens: local_tokens,
                auto_compact_scope_tokens: local_scope,
            }));
        return Err(CodexErr::ContextWindowExceeded);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn try_run_sampling_request<'a>(
    tool_runtime: ToolCallRuntime,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_store: Arc<codex_extension_api::ExtensionData>,
    client_session: &'a mut ModelClientSession,
    responses_metadata: &'a CodexResponsesMetadata,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &'a Prompt,
    generation_id: &'a ModelGenerationId,
    preparation_timing_guard: &'a mut Option<TurnTimingGuard>,
    pending_tool_manifest: Arc<Mutex<Option<codex_protocol::protocol::ToolManifestItem>>>,
    cancellation_token: CancellationToken,
    attempt_progress: &'a mut SamplingAttemptProgress,
) -> impl std::future::Future<Output = CodexResult<SamplingRequestResult>> + 'a {
    Box::pin(async move {
        let mut active_without_pending_passes = 0_u8;
        let next_sample_reason =
            hold_sampling_readiness_for_ordered_prefix(reconcile_turn_progress(
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
        admit_prepared_prompt(&sess, &turn_context, prompt).await?;
        let request_effort = turn_context.request_reasoning_effort();
        let service_tier = service_tier_for_sampling(&sess, &turn_context).await;
        feedback_tags!(
            model = turn_context.model_info.slug.clone(),
            approval_policy = turn_context.approval_policy.value(),
            sandbox_policy = &turn_context.sandbox_policy(),
            effort = request_effort,
            auth_mode = sess.services.auth_manager.auth_mode(),
            features = sess.features.enabled_features(),
        );
        let inference_trace = sess.services.rollout_thread_trace.inference_trace_context(
            turn_context.sub_id.as_str(),
            turn_context.model_info.slug.as_str(),
            turn_context.provider.info().name.as_str(),
        ).with_request_metadata(|| serde_json::json!({
            "enabled_features": sess.features.enabled_features().into_iter().map(Feature::key).collect::<Vec<_>>(),
            "configured_reasoning_effort": turn_context.configured_reasoning_effort,
            "resolved_reasoning_effort": request_effort,
            "reasoning_summary": turn_context.reasoning_summary,
            "service_tier": service_tier,
            "collaboration_mode": turn_context.collaboration_mode,
            "parallel_tool_calls": prompt.parallel_tool_calls,
        }));
        let sampling_timing_guard = turn_context.turn_timing_state.begin_sampling();
        let uses_sequential_cutoff_reasoning_summaries = turn_context
            .config
            .features
            .enabled(Feature::ConcurrentReasoningSummaries)
            && turn_context.provider.info().is_openai();
        client_session.set_turn_timing(Arc::clone(&turn_context.turn_timing_state));
        client_session.set_startup_timing(Arc::clone(&sess.startup_timing));
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
                            request_effort.clone(),
                            turn_context.reasoning_summary,
                            service_tier.clone(),
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
                        StartupPrewarmClaim::Rejected => {
                            ("stale_incompatible", "stale_incompatible")
                        }
                    }
                }
                SessionStartupPrewarmResolution::Unavailable {
                    status: "disabled", ..
                } => ("disabled", "disabled"),
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
        let boundary_session = Arc::clone(&sess);
        let boundary_turn_id = turn_context.sub_id.clone();
        let boundary_turn_timing = Arc::clone(&turn_context.turn_timing_state);
        let (mut boundary_runtime_identity, boundary_harness_build) =
            turn_context.runtime_identity().await;
        if let Some(manifest) = pending_tool_manifest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            boundary_runtime_identity.insert("tool_manifest_v1".to_string(), manifest.hash.clone());
        }
        let bound_context_attempts = Arc::new(Mutex::new(HashSet::new()));
        let boundary_bound_context_attempts = Arc::clone(&bound_context_attempts);
        let boundary_pending_tool_manifest = Arc::clone(&pending_tool_manifest);
        let boundary_workspace_freshness_notices = prompt
            .input
            .iter()
            .flat_map(|item| match item {
                ResponseItem::Message { role, content, .. } if role == "developer" => content
                    .iter()
                    .filter_map(|item| match item {
                        ContentItem::InputText { text }
                            if text.starts_with("<workspace_evidence_invalidation>\n") =>
                        {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        let attempt_prepared: AttemptPreparedCallback = Arc::new(move |identity| {
            let sess = Arc::clone(&boundary_session);
            let turn_id = boundary_turn_id.clone();
            let turn_timing_state = Arc::clone(&boundary_turn_timing);
            let runtime_identity = boundary_runtime_identity.clone();
            let harness_build = boundary_harness_build.clone();
            let bound_context_attempts = Arc::clone(&boundary_bound_context_attempts);
            let pending_tool_manifest = Arc::clone(&boundary_pending_tool_manifest);
            let workspace_freshness_notices = boundary_workspace_freshness_notices.clone();
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
                    let sampling_waiter =
                        SamplingGateWaiterGuard::new(Arc::clone(&turn_timing_state));
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
                        reconcile_turn_progress_event(
                            &turn_timing_state,
                            0,
                            "sampling gate fenced",
                        );
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
                // Checkpointing, ordered persistence, and the rollout flush run on
                // the critical path before every provider dispatch. Attribute them to
                // the persistence phase rather than leaving them inside transport
                // readiness, where they were indistinguishable from connection setup.
                let persistence_guard =
                    turn_timing_state.begin_local_phase(TurnLocalPhase::Persistence);
                let mut timing_checkpoint = turn_timing_state.sampling_checkpoint();
                timing_checkpoint.runtime_identity = runtime_identity;
                timing_checkpoint.harness_build = harness_build;
                let (bound_context, unresolved_context) = sess
                    .bind_context_baseline_candidate(
                        &identity.sampling_request_id,
                        &identity.physical_attempt_id,
                    )
                    .await;
                // Notices accumulate in the prompt, so persisting every text at every
                // boundary grows the rollout quadratically. Persist each text once
                // per rollout; later boundaries keep its position as a digest reference.
                let notice_digests = workspace_freshness_notices
                    .iter()
                    .map(|notice| {
                        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(notice.as_bytes()))
                    })
                    .collect::<Vec<_>>();
                let workspace_freshness_notices = {
                    let persisted = sess
                        .persisted_freshness_notice_digests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    workspace_freshness_notices
                        .into_iter()
                        .zip(&notice_digests)
                        .map(|(notice, digest)| {
                            if persisted.contains(digest) {
                                format!(
                                    "<workspace_evidence_invalidation_ref sha256=\"{digest}\"/>"
                                )
                            } else {
                                notice
                            }
                        })
                        .collect::<Vec<_>>()
                };
                persist_sampling_prefix_before_dispatch(
                    sess.as_ref(),
                    manifest,
                    SamplingBoundaryItem {
                        sampling_request_id: identity.sampling_request_id.clone(),
                        physical_attempt_id: identity.physical_attempt_id.clone(),
                        workspace_freshness_notices,
                        turn_id: Some(turn_id),
                        unresolved_context,
                        timing_checkpoint: Some(timing_checkpoint),
                    },
                )
                .await?;
                // Preserve the accepted sampling prefix before another provider
                // attempt, without joining tools still running in yielded cells.
                // Their all-commits barrier belongs to terminalization; requiring
                // quiescence here can starve sampling while background tools poll.
                sess.flush_rollout().await?;
                sess.persisted_freshness_notice_digests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(notice_digests);
                drop(persistence_guard);
                if bound_context {
                    bound_context_attempts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert((
                            identity.sampling_request_id.clone(),
                            identity.physical_attempt_id.clone(),
                        ));
                }
                turn_timing_state
                    .record_next_sample_block_reason(NextSampleBlockReason::ReadyToSample);
                Ok(sampling_admission)
            })
        });
        // In-memory history already contains the accepted ordered mutations.
        // Surface writer failures here; terminalization owns the durability barrier.
        sess.check_tool_history_persistence()?;
        let stream_result = Box::pin(client_session.stream_with_attempt_prepared(
            prompt,
            &turn_context.model_info,
            &turn_context.session_telemetry,
            request_effort.clone(),
            turn_context.reasoning_summary,
            service_tier,
            responses_metadata,
            &inference_trace,
            Some(attempt_prepared),
        ))
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await;
        drop(model_request_timing_guard);
        let mut stream = stream_result??;
        let attempt_identity = stream.attempt_identity().cloned();
        let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
            FuturesOrdered::new();
        let mut completed_in_flight = VecDeque::new();
        let response_item_recorder = OrderedResponseItemRecorder::default();
        let mut earlier_tool_calls_eligible = true;
        let mut all_tool_calls_eager_read_eligible = true;
        let mut continuation_workspace_prefetch = None;
        let mut needs_follow_up = false;
        let mut tool_result_continuation = false;
        let mut server_end_turn_false = false;
        let mut last_agent_message: Option<String> = None;
        let mut assistant_output = None;
        let mut final_message_candidate = None;
        let mut active_item: Option<TurnItem> = None;
        let mut active_tool_argument_diff_consumer: Option<(
            String,
            Box<dyn ToolArgumentDiffConsumer>,
        )> = None;
        let mut context_baseline_committed = false;
        let mut should_emit_turn_diff = false;
        let mut should_emit_token_count = false;
        let mut latest_models_etag = None;
        let reasoning_effort = request_effort
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "default".to_string());
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
        let mut model_stream_processing_handoff: Option<TurnTimingGuard> = None;
        let outcome: CodexResult<UnsettledSamplingRequestResult> = loop {
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
            // Close the previous event's processing only after this wait opens. A gap
            // between the two guards records an orchestration interval for every
            // streamed event, which fills the exact activity timeline within minutes.
            drop(model_stream_processing_handoff.take());
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
            let stream_event = match stream_event {
                Either::Left(stream_event) => stream_event,
                Either::Right(Some(completion)) => {
                    drop(model_stream_wait_timing_guard);
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
                Some(Err(err)) => {
                    drop(model_stream_wait_timing_guard);
                    if let Some(token_usage) = err.terminal_usage() {
                        turn_context
                            .turn_timing_state
                            .record_generation_token_usage(token_usage.as_ref());
                        if let Err(accounting_error) = sess
                            .update_token_usage_info(&turn_context, token_usage.as_ref())
                            .await
                        {
                            break Err(accounting_error);
                        }
                    }
                    break Err(err.into_response_error());
                }
                None => {
                    break Err(CodexErr::Stream(
                        "stream closed before response.completed".into(),
                        None,
                    ));
                }
            };

            // Open processing before closing the wait, for the same reason.
            model_stream_processing_handoff = turn_context
                .turn_timing_state
                .begin_model_stream_processing(&event);
            drop(model_stream_wait_timing_guard);
            sess.services
                .session_telemetry
                .record_responses(&handle_responses, &event);
            record_turn_ttft_metric(&turn_context, &event);

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
                    if let Some(state) = plan_mode_state.as_mut() {
                        match handle_assistant_item_done_in_plan_mode(
                            &sess,
                            &turn_context,
                            turn_store.as_ref(),
                            &item,
                            state,
                            previously_streamed_item.as_ref(),
                            &mut last_agent_message,
                            &mut assistant_output,
                            &mut final_message_candidate,
                        )
                        .await
                        {
                            Ok(true) => continue,
                            Ok(false) => {}
                            Err(err) => break Err(CodexErr::Io(err)),
                        }
                    }

                    let mut ctx = HandleOutputCtx {
                        sess: sess.clone(),
                        turn_context: turn_context.clone(),
                        turn_store: Arc::clone(&turn_store),
                        tool_runtime: tool_runtime.clone(),
                        cancellation_token: cancellation_token.child_token(),
                        response_item_recorder: response_item_recorder.clone(),
                    };

                    let output_result = match Box::pin(handle_output_item_done(
                        &mut ctx,
                        item,
                        previously_streamed_item,
                        &mut earlier_tool_calls_eligible,
                    ))
                    .instrument(handle_responses)
                    .await
                    {
                        Ok(output_result) => output_result,
                        Err(err) => break Err(err),
                    };
                    if let Some(tool_future) = output_result.tool_future {
                        all_tool_calls_eager_read_eligible &=
                            output_result.read_only_prefetch_eligible;
                        // Poll every accepted call while the response is streaming. The
                        // runtime still owns repository gates, approvals and cancellation;
                        // FuturesOrdered preserves provider order when relaying results.
                        if output_result.eager_read_eligible {
                            in_flight.push_back(start_eager_tool_future(tool_future));
                        } else {
                            in_flight.push_back(tool_future.into_future().boxed());
                        }
                    }
                    if let Some(agent_message) = output_result.last_agent_message {
                        last_agent_message = Some(agent_message);
                    }
                    if let Some(output) = output_result.assistant_output {
                        if let Some(candidate) = final_message_candidate_from_output(output.clone())
                        {
                            update_final_message_candidate(&mut final_message_candidate, candidate);
                        }
                        assistant_output = Some(output);
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
                            let mut seeded = assistant_message_stream_parsers
                                .seed_item_text(&item_id, &raw_text);
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
                                let _ = emit_streamed_assistant_text_delta(
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
                ResponseEvent::ServerReasoningIncluded(_) => {}
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
                    response_id,
                    token_usage,
                    end_turn,
                    ..
                } => {
                    if let Some(identity) = attempt_identity.as_ref() {
                        turn_context.extension_data.insert(identity.clone());
                        turn_context
                            .turn_timing_state
                            .record_model_response_id(&identity.physical_attempt_id, &response_id);
                    }
                    sess.mark_tool_history_consumed(
                        &turn_context,
                        client_session.effective_input().unwrap_or(&prompt.input),
                        generation_id.clone(),
                    )
                    .await;
                    turn_context
                        .turn_timing_state
                        .record_generation_token_usage(token_usage.as_ref());
                    let remaining_deltas = flush_assistant_text_segments_all(
                        &sess,
                        &turn_context,
                        plan_mode_state.as_mut(),
                        &mut assistant_message_stream_parsers,
                    )
                    .await;
                    if let Some(active) = active_item.as_mut() {
                        for delta in remaining_deltas {
                            if delta.item_id == active.id() {
                                apply_partial_agent_message_delta(active, &delta);
                            }
                        }
                    }
                    // Missing usage must describe the committed response prefix.
                    // Tools still execute concurrently; auxiliary delivery is joined below.
                    if token_usage.is_none()
                        && let Err(error) = response_item_recorder.flush_required().await
                    {
                        cancellation_token.cancel();
                        break Err(error);
                    }
                    // Train using the prepared content recipe; timing diagnostics
                    // continue to reject ambiguous attempts and missing metadata.
                    let (effective_input, prefix) = client_session
                        .effective_input_shared()
                        .map(|input| {
                            (
                                input,
                                if turn_context.model_info.use_responses_lite {
                                    usize::from(!prompt.base_instructions.text.is_empty()) + 1
                                } else {
                                    0
                                },
                            )
                        })
                        .unwrap_or_else(|| (Arc::clone(&prompt.input), 0));
                    turn_context
                        .extension_data
                        .insert(prompt.calibration_basis_with_prefix(effective_input, prefix));
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
                    if needs_follow_up
                        && !in_flight.is_empty()
                        && all_tool_calls_eager_read_eligible
                    {
                        let history = sess.clone_history().await;
                        continuation_workspace_prefetch = start_continuation_workspace_prefetch(
                            &history,
                            &turn_diff_tracker,
                            Arc::clone(&sess.services.git_workspace),
                            turn_context.config.cwd.clone(),
                            turn_context.environments.clone(),
                        )
                        .await;
                    }
                    break Ok(UnsettledSamplingRequestResult {
                        needs_follow_up,
                        last_agent_message,
                        assistant_output,
                        tool_result_continuation,
                        server_end_turn_false,
                    });
                }
                ResponseEvent::OutputTextDelta(delta) => {
                    // In review child threads, suppress assistant text deltas; the
                    // UI will show a selection popup from the final ReviewOutput.
                    if let Some(active) = active_item.as_mut() {
                        if !active_item_is_streaming_to_client {
                            continue;
                        }
                        if let TurnItem::AgentMessage(message) = active {
                            let item_id = message.id.as_str();
                            let parsed =
                                assistant_message_stream_parsers.parse_delta(item_id, &delta);
                            if let Some(event) = emit_streamed_assistant_text_delta(
                                &sess,
                                &turn_context,
                                plan_mode_state.as_mut(),
                                item_id,
                                parsed,
                            )
                            .await
                            {
                                apply_partial_agent_message_delta(active, &event);
                            }
                        } else {
                            let event = AgentMessageContentDeltaEvent {
                                thread_id: sess.thread_id.to_string(),
                                turn_id: turn_context.sub_id.clone(),
                                item_id: active.id(),
                                delta,
                            };
                            sess.send_event(
                                &turn_context,
                                EventMsg::AgentMessageContentDelta(event),
                            )
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
                    let Some((active_call_id, consumer)) =
                        active_tool_argument_diff_consumer.as_mut()
                    else {
                        continue;
                    };
                    let call_id = match call_id {
                        Some(call_id) if call_id.as_str() != active_call_id.as_str() => continue,
                        Some(call_id) => call_id,
                        None => active_call_id.clone(),
                    };
                    if let Some(event) =
                        consumer.consume_diff(turn_context.as_ref(), call_id, &delta)
                    {
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
                    if let Some(active) = active_item.as_mut() {
                        if !active_item_is_streaming_to_client {
                            continue;
                        }
                        if let TurnItem::Reasoning(reasoning) = active {
                            append_partial_reasoning_text(
                                &mut reasoning.summary_text,
                                summary_index,
                                &delta,
                            );
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
                    let Some(active) = active_item.as_mut() else {
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
                    if let TurnItem::Reasoning(reasoning) = active {
                        append_partial_reasoning_text(
                            &mut reasoning.summary_text,
                            summary_index,
                            &text,
                        );
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
                    if let Some(active) = active_item.as_mut() {
                        if !active_item_is_streaming_to_client {
                            continue;
                        }
                        if let TurnItem::Reasoning(reasoning) = active {
                            append_partial_reasoning_text(
                                &mut reasoning.raw_content,
                                content_index,
                                &delta,
                            );
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
        drop(model_stream_processing_handoff);
        drop(sampling_timing_guard);

        let remaining_deltas = flush_assistant_text_segments_all(
            &sess,
            &turn_context,
            plan_mode_state.as_mut(),
            &mut assistant_message_stream_parsers,
        )
        .await;
        if let Some(active) = active_item.as_mut() {
            for delta in remaining_deltas {
                if delta.item_id == active.id() {
                    apply_partial_agent_message_delta(active, &delta);
                }
            }
        }
        // Close only items that were actually streamed. These partial snapshots close
        // the item stream; the unchanged turn result still reports failure or interruption.
        if let Some(state) = plan_mode_state.as_mut() {
            complete_partial_plan_items(&sess, &turn_context, state).await;
        }
        if active_item_is_streaming_to_client
            && let Some(item) = active_item.take()
            && (plan_mode_state.is_none() || !matches!(item, TurnItem::AgentMessage(_)))
        {
            sess.emit_turn_item_completed(&turn_context, item).await;
        }
        // Join ordered response recording before publishing tool outputs. Execution
        // already overlaps the stream and does not wait for this response-wide flush.
        if let Err(error) = response_item_recorder.flush().await {
            // Execution may already have started, but outputs must not be relayed
            // without their recorded call prefix. Cancel remaining owned work.
            cancellation_token.cancel();
            return Err(error);
        }

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
        let generation_workspace_evidence =
            tool_runtime.flush_workspace_evidence_generation().await?;
        let required_tool_terminal = required_tool_terminal?;

        // Tool completion is the earliest safe boundary after mutations. Start a
        // replacement capture here, overlapping terminal/event bookkeeping, instead
        // of discovering a stale prefetch during the next prompt's preparation.
        if needs_follow_up
            && required_tool_terminal.is_none()
            && generation_workspace_evidence
                .prefetched_workspace_identity
                .is_none()
            && !cancellation_token.is_cancelled()
        {
            let revision = turn_diff_tracker.lock().await.current_mutation_revision();
            if continuation_workspace_prefetch
                .as_ref()
                .is_none_or(|(baseline, _)| {
                    !continuation_workspace_prefetch_is_current(*baseline, revision, false)
                })
            {
                // Drop (and abort) the stale capture before starting its replacement.
                drop(continuation_workspace_prefetch.take());
                let history = sess.clone_history().await;
                continuation_workspace_prefetch = start_continuation_workspace_prefetch(
                    &history,
                    &turn_diff_tracker,
                    Arc::clone(&sess.services.git_workspace),
                    turn_context.config.cwd.clone(),
                    turn_context.environments.clone(),
                )
                .await;
            }
        }

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
                attributed_mutation_revision: tracker.attributed_mutation_revision(),
                tool_exposure_revision: turn_context.deferred_tool_activation_revision(),
            }
        };
        let prefetched_workspace_identity =
            generation_workspace_evidence.prefetched_workspace_identity;
        // Keep the capture in flight across the continuation's step-context capture.
        // The turn owns cancellation and rechecks its revision before prompt preparation.
        let continuation_workspace_prefetch =
            continuation_workspace_prefetch.filter(|(baseline, _)| {
                prefetched_workspace_identity.is_none()
                    && continuation_workspace_prefetch_is_current(
                        *baseline,
                        settled_state.mutation_revision,
                        false,
                    )
            });
        let outcome = outcome.map(|result| SamplingRequestResult {
            needs_follow_up: result.needs_follow_up,
            last_agent_message: result.last_agent_message,
            assistant_output: result.assistant_output,
            final_message_candidate: final_message_candidate.map(|mut candidate| {
                candidate.completion_attempt_id =
                    format!("{}:{}", generation_id.turn_id, generation_id.ordinal);
                candidate
            }),
            settled_state,
            tool_result_continuation: result.tool_result_continuation,
            server_end_turn_false: result.server_end_turn_false,
            required_tool_terminal,
            prefetched_workspace_identity,
            continuation_workspace_prefetch,
        });

        if should_emit_turn_diff {
            let (unified_diff, invalidation_warning) = {
                let mut tracker = turn_diff_tracker.lock().await;
                (
                    tracker.take_unified_diff_if_changed(),
                    tracker.take_invalidation_warning(),
                )
            };
            if let Some(message) = invalidation_warning {
                sess.send_event(
                    &turn_context,
                    EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                        message: message.to_string(),
                    }),
                )
                .await;
            }
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
    })
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

#[cfg(test)]
#[path = "token_budget_admission_tests.rs"]
mod token_budget_admission_tests;

#[cfg(test)]
#[path = "turn_output_tests.rs"]
mod turn_output_tests;
