//! Tool-router preparation, reuse and exposure identity for model requests.
//!
//! Owns router construction and its session cache. The turn loop decides when
//! to prepare or retry; request_scaffold owns model-visible prompt projection.

use std::sync::Arc;

use crate::connectors;
use crate::mcp_tool_exposure::build_mcp_tool_exposure;
use crate::mcp_tool_exposure::resolve_selected_skill_mcp_exposure;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::ToolRouter;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::DirectMcpToolEntrypoint;
use crate::tools::exposure::DynamicToolExposureIdentity;
use crate::tools::exposure::EnvironmentSurfaceMode;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::router::ToolRouterParams;
use crate::tools::router::ToolSuggestCandidates;
use crate::tools::router::ToolSuggestPresentation;
use crate::tools::router::extension_tool_surface_revision;
use crate::tools::router::extension_tool_surface_snapshot;
use crate::tools::spec_plan::search_tool_enabled;
use crate::tools::spec_plan::tool_suggest_enabled;
use crate::turn_timing::TurnLocalPhase;
use codex_analytics::SkillInvocation;
use codex_async_utils::OrCancelExt;
use codex_core_plugins::PluginLoadOutcome;
use codex_core_plugins::RecommendedPluginCandidatesInput;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::ToolName;
use codex_tools::request_user_input_available_modes;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

pub(super) async fn finalized_router_matches_current_exposure(
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
pub(in crate::session) struct SessionPreparedRouterCache {
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

pub(super) async fn built_tools_for_pending_turn(
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
    if turn_context.config.web_search_mode.value()
        != codex_protocol::config_types::WebSearchMode::Disabled
    {
        turn_context
            .provider
            .prepare_hosted_web_search()
            .or_cancel(cancellation_token)
            .await??;
    }
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
        && available_modes.contains(&turn_context.collaboration_mode.mode)
}

pub(in crate::session) fn agent_surface_stage(sess: &Session, turn_context: &TurnContext) -> AgentSurfaceStage {
    let spawn_eligible = turn_context.multi_agent_version != MultiAgentVersion::Disabled;
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

