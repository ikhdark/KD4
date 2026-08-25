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
    pub(crate) wait_available: bool,
    pub(crate) goal_surface_state: GoalSurfaceState,
    pub(crate) extension_tool_surface_revision: u64,
    pub(crate) mcp_resources_available: bool,
    pub(crate) tool_search_available: bool,
    pub(crate) request_user_input_eligible: bool,
    pub(crate) environment_mode: EnvironmentSurfaceMode,
    pub(crate) environment_starting: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DynamicToolExposureIdentity {
    pub(crate) agent_surface_stage: AgentSurfaceStage,
    pub(crate) wait_available: bool,
    pub(crate) extension_tool_surface_revision: u64,
    pub(crate) mcp_resources_available: bool,
    pub(crate) environment_mode: EnvironmentSurfaceMode,
    pub(crate) environment_starting: bool,
}

impl ToolExposureIdentity {
    pub(crate) fn dynamic_identity(&self) -> DynamicToolExposureIdentity {
        DynamicToolExposureIdentity {
            agent_surface_stage: self.agent_surface_stage,
            wait_available: self.wait_available,
            extension_tool_surface_revision: self.extension_tool_surface_revision,
            mcp_resources_available: self.mcp_resources_available,
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
            wait_available: true,
            goal_surface_state: GoalSurfaceState::Active,
            extension_tool_surface_revision: 0,
            mcp_resources_available: true,
            tool_search_available: false,
            request_user_input_eligible: true,
            environment_mode: EnvironmentSurfaceMode::One,
            environment_starting: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_changes_only_at_declared_coarse_exposure_transitions() {
        let base = ToolExposureIdentity {
            selected_skill_direct_mcp_entrypoints: Vec::new(),
            agent_surface_stage: AgentSurfaceStage::SpawnOnly,
            wait_available: false,
            goal_surface_state: GoalSurfaceState::Disabled,
            extension_tool_surface_revision: 0,
            mcp_resources_available: false,
            tool_search_available: false,
            request_user_input_eligible: false,
            environment_mode: EnvironmentSurfaceMode::None,
            environment_starting: false,
        };
        assert_eq!(base, base.clone());

        let mut changed = base.clone();
        changed.wait_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.goal_surface_state = GoalSurfaceState::Inactive;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.extension_tool_surface_revision = 1;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.mcp_resources_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.tool_search_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.request_user_input_eligible = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.agent_surface_stage = AgentSurfaceStage::Lifecycle;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.environment_mode = EnvironmentSurfaceMode::One;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.environment_starting = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed
            .selected_skill_direct_mcp_entrypoints
            .push(DirectMcpToolEntrypoint {
                server_name: "repo-atlas".to_string(),
                tool_name: "task".to_string(),
            });
        assert_ne!(base, changed);
    }

    #[test]
    fn zero_one_or_many_counts_collapse_to_boolean_identity() {
        let one_waitable_cell = ToolExposureIdentity {
            wait_available: true,
            ..ToolExposureIdentity::default()
        };
        let two_waitable_cells = one_waitable_cell.clone();
        assert_eq!(one_waitable_cell, two_waitable_cells);

        let one_resource_server = ToolExposureIdentity {
            mcp_resources_available: true,
            ..ToolExposureIdentity::default()
        };
        let two_resource_servers = one_resource_server.clone();
        assert_eq!(one_resource_server, two_resource_servers);
    }

    #[test]
    fn dynamic_identity_excludes_turn_frozen_discovery_inputs() {
        let base = ToolExposureIdentity::default();
        let mut static_change = base.clone();
        static_change.tool_search_available = !static_change.tool_search_available;
        static_change.request_user_input_eligible = !static_change.request_user_input_eligible;
        static_change
            .selected_skill_direct_mcp_entrypoints
            .push(DirectMcpToolEntrypoint {
                server_name: "server".to_string(),
                tool_name: "tool".to_string(),
            });
        assert_eq!(base.dynamic_identity(), static_change.dynamic_identity());

        let mut dynamic_change = base.clone();
        dynamic_change.extension_tool_surface_revision = 7;
        assert_ne!(base.dynamic_identity(), dynamic_change.dynamic_identity());
    }
}
