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
pub(crate) enum TaskToolPhase {
    Discovery,
    Implementation,
    Validation,
    Completion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalSurfaceState {
    Disabled,
    Inactive,
    Active,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct DirectMcpToolEntrypoint {
    pub(crate) server_name: String,
    pub(crate) tool_name: String,
}

/// Coarse inputs that can change which schemas are model-visible.
///
/// Only capability state that changes the model-visible schema belongs here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolExposureIdentity {
    pub(crate) selected_skill_direct_mcp_entrypoints: Vec<DirectMcpToolEntrypoint>,
    pub(crate) agent_surface_stage: AgentSurfaceStage,
    pub(crate) task_tool_phase: TaskToolPhase,
    pub(crate) agent_spawn_available: bool,
    pub(crate) wait_available: bool,
    pub(crate) unified_exec_resume_available: bool,
    pub(crate) goal_surface_state: GoalSurfaceState,
    pub(crate) mcp_resources_available: bool,
    pub(crate) request_user_input_eligible: bool,
}

impl Default for ToolExposureIdentity {
    fn default() -> Self {
        // Focused router tests that do not construct session state retain the historical surface.
        Self {
            selected_skill_direct_mcp_entrypoints: Vec::new(),
            agent_surface_stage: AgentSurfaceStage::TypedAdministration,
            task_tool_phase: TaskToolPhase::Implementation,
            agent_spawn_available: true,
            wait_available: true,
            unified_exec_resume_available: true,
            goal_surface_state: GoalSurfaceState::Active,
            mcp_resources_available: true,
            request_user_input_eligible: true,
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
            task_tool_phase: TaskToolPhase::Discovery,
            agent_spawn_available: false,
            wait_available: false,
            unified_exec_resume_available: false,
            goal_surface_state: GoalSurfaceState::Disabled,
            mcp_resources_available: false,
            request_user_input_eligible: false,
        };
        assert_eq!(base, base.clone());

        let mut changed = base.clone();
        changed.agent_spawn_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.wait_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.unified_exec_resume_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.goal_surface_state = GoalSurfaceState::Inactive;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.mcp_resources_available = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.request_user_input_eligible = true;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.agent_surface_stage = AgentSurfaceStage::Lifecycle;
        assert_ne!(base, changed);

        let mut changed = base.clone();
        changed.task_tool_phase = TaskToolPhase::Validation;
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
