use super::EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
use super::session::Session;
use super::step_context::StepContext;
use crate::connectors;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::AppsInstructionsState;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::PluginsInstructionsState;
use crate::context::world_state::SourceClosureWorldState;
use crate::context::world_state::WorldState;
use codex_extension_api::WorldStateContributionInput;
use futures::StreamExt;
use futures::stream::FuturesOrdered;

impl Session {
    #[tracing::instrument(name = "world_state.build", level = "info", skip_all)]
    pub(crate) async fn build_world_state_for_step(
        &self,
        step_context: &StepContext,
    ) -> WorldState {
        self.build_world_state_for_step_with_mode(step_context, false)
            .await
    }

    pub(crate) async fn estimate_world_state_for_step(
        &self,
        step_context: &StepContext,
    ) -> WorldState {
        self.build_world_state_for_step_with_mode(step_context, true)
            .await
    }

    async fn build_world_state_for_step_with_mode(
        &self,
        step_context: &StepContext,
        estimate: bool,
    ) -> WorldState {
        let turn_context = step_context.turn.as_ref();
        tracing::trace!(
            selected_capability_root_count = step_context.selected_capability_roots.len(),
            "building step world state"
        );
        let environment_subagents = if turn_context.config.include_environment_context {
            self.services
                .agent_control
                .format_environment_context_subagents(self.thread_id)
                .await
        } else {
            String::new()
        };

        let mut world_state = WorldState::default();
        world_state.add_section(AgentsMdState::new(step_context.loaded_agents_md.as_deref()));
        if turn_context.config.include_environment_context {
            world_state.add_section(
                EnvironmentsState::from_turn_context_with_environments(
                    turn_context,
                    &step_context.environments,
                )
                .with_subagents(environment_subagents),
            );
        }
        let apps_available =
            if turn_context.config.include_apps_instructions && turn_context.apps_enabled() {
                let tools = step_context.mcp_tools().await;
                connectors::with_app_enabled_state(
                    connectors::accessible_connectors_from_mcp_tools(tools),
                    &turn_context.config,
                )
                .into_iter()
                .any(|connector| connector.is_accessible && connector.is_enabled)
            } else {
                false
            };
        world_state.add_section(AppsInstructionsState::new(apps_available));
        world_state.add_section(PluginsInstructionsState::new(
            step_context.mcp.plugins_available(),
        ));
        let source_closure_summary = turn_context.source_closure.lock().await.summary();
        world_state.add_section(SourceClosureWorldState::new(source_closure_summary));
        let environments = step_context.environments.to_selections();
        let ready_selected_capability_roots = step_context
            .selected_capability_roots
            .iter()
            .map(|root| root.selected_root().clone())
            .collect::<Vec<_>>();
        // World-state contributors are independent. Poll them concurrently while preserving
        // registration order, and do not let an optional contributor block the model request.
        let deadline = tokio::time::Instant::now() + EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
        let mut pending = FuturesOrdered::new();
        for (contributor_index, contributor) in self
            .services
            .extensions
            .context_contributors()
            .iter()
            .enumerate()
        {
            let input = WorldStateContributionInput {
                thread_id: self.thread_id(),
                turn_id: turn_context.sub_id.as_str(),
                environments: &environments,
                ready_selected_capability_roots: &ready_selected_capability_roots,
                session_store: &self.services.session_extension_data,
                thread_store: &self.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
            };
            let contribution = if estimate {
                contributor.estimate_world_state(input)
            } else {
                contributor.contribute_world_state(input)
            };
            pending.push_back(async move {
                match tokio::time::timeout_at(deadline, contribution).await {
                    Ok(sections) => sections,
                    Err(_) => {
                        tracing::warn!(
                            contributor_index,
                            scope = "world_state",
                            timeout = ?EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT,
                            "extension context contributor timed out; omitting its sections"
                        );
                        Vec::new()
                    }
                }
            });
        }
        while let Some(sections) = pending.next().await {
            for section in sections {
                world_state.add_extension_section(section);
            }
        }
        world_state
    }
}
