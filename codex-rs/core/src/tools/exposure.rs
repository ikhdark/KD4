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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct NativeEnvironmentContract {
    pub(crate) explicit_addressing: bool,
    pub(crate) primary_foreign_cwd: bool,
    pub(crate) single_local: bool,
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
    pub(crate) windows_shell_guidance: bool,
    pub(crate) native_environment_contract: NativeEnvironmentContract,
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
    pub(crate) windows_shell_guidance: bool,
    pub(crate) native_environment_contract: NativeEnvironmentContract,
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
            windows_shell_guidance: self.windows_shell_guidance,
            native_environment_contract: self.native_environment_contract,
        }
    }
}

impl Default for ToolExposureIdentity {
    fn default() -> Self {
        // Callers without session state retain the historical surface.
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
            windows_shell_guidance: true,
            native_environment_contract: NativeEnvironmentContract::default(),
        }
    }
}

