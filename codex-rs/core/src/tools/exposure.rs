use codex_protocol::config_types::ModeKind;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentSurfaceStage {
    Prohibited,
    SpawnOnly,
    Lifecycle,
    TypedAdministration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalSurfaceState {
    Disabled,
    Inactive,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EnvironmentSurfaceMode {
    None,
    One,
    Multiple,
}

impl EnvironmentSurfaceMode {
    pub(crate) fn from_count(count: usize) -> Self {
        match count {
            0 => Self::None,
            1 => Self::One,
            _ => Self::Multiple,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct DirectMcpToolEntrypoint {
    pub(crate) server_name: String,
    pub(crate) tool_name: String,
}

/// Coarse inputs that can change which schemas are model-visible.
///
/// Runtime authorization and fine-grained subsystem state deliberately do not belong here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolExposureIdentity {
    pub(crate) selected_skill_direct_mcp_entrypoints: Vec<DirectMcpToolEntrypoint>,
    pub(crate) agent_surface_stage: AgentSurfaceStage,
    pub(crate) goal_surface_state: GoalSurfaceState,
    pub(crate) extension_tool_surface_revision: u64,
    pub(crate) mcp_tool_catalog_revision: u64,
    pub(crate) mcp_resources_available: bool,
    pub(crate) tool_search_available: bool,
    pub(crate) request_user_input_eligible: bool,
    pub(crate) collaboration_mode: ModeKind,
    pub(crate) environment_mode: EnvironmentSurfaceMode,
    pub(crate) environment_starting: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DynamicToolExposureIdentity {
    pub(crate) agent_surface_stage: AgentSurfaceStage,
    pub(crate) extension_tool_surface_revision: u64,
    pub(crate) mcp_tool_catalog_revision: u64,
    pub(crate) mcp_resources_available: bool,
    pub(crate) request_user_input_eligible: bool,
    pub(crate) collaboration_mode: ModeKind,
    pub(crate) environment_mode: EnvironmentSurfaceMode,
    pub(crate) environment_starting: bool,
}

impl ToolExposureIdentity {
    pub(crate) fn dynamic_identity(&self) -> DynamicToolExposureIdentity {
        DynamicToolExposureIdentity {
            agent_surface_stage: self.agent_surface_stage,
            extension_tool_surface_revision: self.extension_tool_surface_revision,
            mcp_tool_catalog_revision: self.mcp_tool_catalog_revision,
            mcp_resources_available: self.mcp_resources_available,
            request_user_input_eligible: self.request_user_input_eligible,
            collaboration_mode: self.collaboration_mode,
            environment_mode: self.environment_mode,
            environment_starting: self.environment_starting,
        }
    }
}

impl Default for ToolExposureIdentity {
    fn default() -> Self {
        // Focused router tests that do not construct session state retain the historical surface.
        Self {
            selected_skill_direct_mcp_entrypoints: Vec::new(),
            agent_surface_stage: AgentSurfaceStage::TypedAdministration,
            goal_surface_state: GoalSurfaceState::Active,
            extension_tool_surface_revision: 0,
            mcp_tool_catalog_revision: 0,
            mcp_resources_available: true,
            tool_search_available: false,
            request_user_input_eligible: true,
            collaboration_mode: ModeKind::Default,
            environment_mode: EnvironmentSurfaceMode::One,
            environment_starting: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identity_changes_only_at_declared_coarse_exposure_transitions() {
        use crate::session::step_context::StepContext;
        use crate::session::tests::make_session_and_context;
        use crate::tools::handlers::ToolSearchHandlerCache;
        use crate::tools::router::ToolRouter;
        use crate::tools::router::ToolRouterParams;
        use std::sync::Arc;

        let (_session, turn) = make_session_and_context().await;
        let step = StepContext::for_test(Arc::new(turn));
        let cache = ToolSearchHandlerCache::default();
        // Reuse the actual planning consumer and its cache across both directions
        // of the transition; stale registration or visibility must not survive.
        for eligible in [false, true, false] {
            let router = ToolRouter::from_context(
                step.as_ref(),
                ToolRouterParams {
                    mcp_tools: None,
                    deferred_mcp_tools: None,
                    tool_suggest_candidates: None,
                    extension_tool_executors: Vec::new(),
                    dynamic_tools: &[],
                    exposure_identity: ToolExposureIdentity {
                        request_user_input_eligible: eligible,
                        ..ToolExposureIdentity::default()
                    },
                },
                &cache,
            );
            assert_eq!(
                router
                    .registered_tool_names_for_test()
                    .iter()
                    .any(|name| name.to_string() == "request_user_input"),
                eligible,
                "registration must follow current eligibility",
            );
            assert_eq!(
                router
                    .model_visible_specs()
                    .iter()
                    .any(|spec| spec.name() == "request_user_input"),
                eligible,
                "model-visible schemas must follow current eligibility",
            );
        }
    }

    #[test]
    fn zero_one_or_many_counts_collapse_to_boolean_identity() {
        for (count, expected) in [
            (0, EnvironmentSurfaceMode::None),
            (1, EnvironmentSurfaceMode::One),
            (2, EnvironmentSurfaceMode::Multiple),
            (usize::MAX, EnvironmentSurfaceMode::Multiple),
        ] {
            assert_eq!(EnvironmentSurfaceMode::from_count(count), expected);
        }
    }

    #[test]
    fn dynamic_identity_excludes_turn_frozen_discovery_inputs() {
        let base = ToolExposureIdentity::default();
        let mut static_change = base.clone();
        static_change.tool_search_available = !static_change.tool_search_available;
        static_change
            .selected_skill_direct_mcp_entrypoints
            .push(DirectMcpToolEntrypoint {
                server_name: "server".to_string(),
                tool_name: "tool".to_string(),
            });
        assert_eq!(base.dynamic_identity(), static_change.dynamic_identity());

        let mut request_user_input_change = base.clone();
        request_user_input_change.request_user_input_eligible =
            !request_user_input_change.request_user_input_eligible;
        assert_ne!(
            base.dynamic_identity(),
            request_user_input_change.dynamic_identity()
        );

        let mut collaboration_mode_change = base.clone();
        collaboration_mode_change.collaboration_mode = ModeKind::Plan;
        assert_ne!(
            base.dynamic_identity(),
            collaboration_mode_change.dynamic_identity()
        );

        let mut dynamic_change = base.clone();
        dynamic_change.extension_tool_surface_revision = 7;
        assert_ne!(base.dynamic_identity(), dynamic_change.dynamic_identity());

        let mut catalog_change = base.clone();
        catalog_change.mcp_tool_catalog_revision = 1;
        assert_ne!(base.dynamic_identity(), catalog_change.dynamic_identity());
    }
}
