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
    pub(crate) mcp_resources_available: bool,
    pub(crate) tool_search_available: bool,
    pub(crate) request_user_input_eligible: bool,
    pub(crate) environment_mode: EnvironmentSurfaceMode,
    pub(crate) environment_starting: bool,
}

impl Default for ToolExposureIdentity {
    fn default() -> Self {
        // Focused router tests that do not construct session state retain the historical surface.
        Self {
            selected_skill_direct_mcp_entrypoints: Vec::new(),
            agent_surface_stage: AgentSurfaceStage::TypedAdministration,
            wait_available: true,
            goal_surface_state: GoalSurfaceState::Active,
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
}
