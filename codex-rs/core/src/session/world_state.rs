use super::EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
use super::session::Session;
use super::step_context::StepContext;
use crate::connectors;
use crate::context::world_state::AgentsMdState;
use crate::context::world_state::AppsInstructionsState;
use crate::context::world_state::EnvironmentsState;
use crate::context::world_state::PluginsInstructionsState;
use crate::context::world_state::SubagentsState;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
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

        let previous_world_state = self.state.lock().await.history.world_state_baseline();
        let recovered_agents =
            if step_context.agents_md_freshness != crate::agents_md::AgentsMdFreshness::Refreshed {
                self.services.agents_md_manager.recovered_snapshot().await
            } else {
                None
            };
        let mut world_state = WorldState::default();
        world_state.add_section(
            AgentsMdState::new_cached(
                step_context.loaded_agents_md.as_deref(),
                step_context.agents_md_stable_context.as_ref(),
                step_context.agents_md_freshness,
            )
            .with_unavailable_fallback(|| {
                previous_world_state
                    .as_ref()
                    .and_then(|previous| previous.section("agents_md"))
                    .cloned()
                    .or(recovered_agents)
            }),
        );
        if turn_context.config.include_environment_context {
            world_state.add_section(EnvironmentsState::from_turn_context_with_environments(
                turn_context,
                &step_context.environments,
            ));
            if !environment_subagents.is_empty()
                || previous_world_state
                    .as_ref()
                    .is_some_and(|state| state.section("subagents").is_some())
            {
                world_state.add_section(SubagentsState::new(environment_subagents));
            }
        }
        // A model whose catalog entry says it needs no usage guidance gets
        // none. Guidance an earlier request already delivered stays tracked, so
        // a later loss of the capability is still reported; an untracked
        // section says nothing, which never claims the capability is gone.
        let tracked = |id: &str| {
            previous_world_state
                .as_ref()
                .is_some_and(|state| state.section(id).is_some())
        };
        if turn_context.model_info.include_apps_usage_instructions || tracked("apps_instructions") {
            let apps_available =
                if turn_context.config.include_apps_instructions && turn_context.apps_enabled() {
                    if step_context.mcp_tool_snapshot().await.temporarily_unavailable {
                        // A timed-out capture advertises no tools by construction; it
                        // is no evidence that Apps disappeared, so keep the delivered state.
                        apps_availability_for_unavailable_capture(previous_world_state.as_ref())
                    } else {
                        let tools = step_context.mcp_tools().await;
                        connectors::with_app_enabled_state(
                            connectors::accessible_connectors_from_mcp_tools(tools),
                            &turn_context.config,
                        )
                        .into_iter()
                        .any(|connector| connector.is_accessible && connector.is_enabled)
                    }
                } else {
                    false
                };
            world_state.add_section(AppsInstructionsState::new(apps_available));
        }
        if turn_context.model_info.include_plugin_usage_instructions
            || tracked("plugins_instructions")
        {
            world_state.add_section(PluginsInstructionsState::new(
                step_context.mcp.plugins_available(),
            ));
        }
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
            let owned_section_ids = contributor.world_state_section_ids();
            let input = WorldStateContributionInput {
                thread_id: self.thread_id(),
                turn_id: turn_context.sub_id.as_str(),
                environments: &environments,
                ready_selected_capability_roots: &ready_selected_capability_roots,
                bound_selected_capability_roots: &step_context.selected_capability_roots,
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
                (
                    contributor_index,
                    owned_section_ids,
                    tokio::time::timeout_at(deadline, contribution).await,
                )
            });
        }
        while let Some((contributor_index, owned_section_ids, result)) = pending.next().await {
            match result {
                Ok(sections) => {
                    for id in owned_section_ids {
                        if !sections.iter().any(|section| section.id() == *id)
                            && let Some(rendering) = previous_world_state
                                .as_ref()
                                .and_then(|previous| previous.rendered_extension(id))
                        {
                            world_state.add_removed_extension_section(id, rendering);
                        }
                    }
                    for section in sections {
                        world_state.add_extension_section(section);
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        contributor_index,
                        scope = "world_state",
                        timeout = ?EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT,
                        "extension context contributor timed out; preserving its last accepted sections"
                    );
                    for id in owned_section_ids {
                        if let Some(snapshot) = previous_world_state
                            .as_ref()
                            .and_then(|previous| previous.section(id))
                        {
                            let rendering = previous_world_state
                                .as_ref()
                                .and_then(|previous| previous.rendered_extension(id))
                                .cloned();
                            world_state.add_preserved_extension_section(
                                id,
                                snapshot.clone(),
                                rendering,
                            );
                        }
                    }
                }
            }
        }
        world_state
    }
}

/// Apps availability for a step whose MCP capture timed out. The placeholder
/// catalog is empty, so reuse the delivered state rather than retracting
/// guidance; a session that never received the guidance stays unavailable.
fn apps_availability_for_unavailable_capture(previous: Option<&WorldStateSnapshot>) -> bool {
    previous
        .and_then(|state| state.section("apps_instructions"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod apps_capture_tests {
    use super::apps_availability_for_unavailable_capture;
    use crate::context::AppsInstructionsUnavailable;
    use crate::context::ContextualUserFragment;
    use crate::context::world_state::AppsInstructionsState;
    use crate::context::world_state::WorldState;
    use crate::context::world_state::WorldStateSnapshot;
    use pretty_assertions::assert_eq;

    fn delivered(available: bool) -> WorldStateSnapshot {
        let mut state = WorldState::default();
        state.add_section(AppsInstructionsState::new(available));
        state.snapshot()
    }

    #[test]
    fn timed_out_capture_keeps_delivered_apps_guidance() {
        let delivered = delivered(/*available*/ true);
        let preserved = apps_availability_for_unavailable_capture(Some(&delivered));
        assert!(preserved);
        let mut current = WorldState::default();
        current.add_section(AppsInstructionsState::new(preserved));
        assert!(
            current.render_diff(&delivered).is_empty(),
            "a timed-out MCP capture must not retract delivered Apps guidance"
        );
    }

    #[test]
    fn observed_apps_loss_still_retracts_guidance() {
        // A coherent capture without accessible connectors remains authoritative.
        let mut current = WorldState::default();
        current.add_section(AppsInstructionsState::new(/*available*/ false));
        assert_eq!(
            current
                .render_diff(&delivered(/*available*/ true))
                .into_iter()
                .map(|fragment| fragment.render())
                .collect::<Vec<_>>(),
            vec![AppsInstructionsUnavailable.render()]
        );
        // A timeout never invents guidance that was not delivered.
        assert!(!apps_availability_for_unavailable_capture(Some(&delivered(
            /*available*/ false
        ))));
        assert!(!apps_availability_for_unavailable_capture(None));
    }
}
