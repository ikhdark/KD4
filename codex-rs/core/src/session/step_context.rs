use std::sync::Arc;
use std::sync::OnceLock;

use crate::agents_md::LoadedAgentsMd;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::McpRuntimeSnapshot;
use crate::session::turn_context::TurnContext;
use crate::tools::router::ToolRouter;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::ToolInfo;
use tokio::sync::OnceCell;

/// Request-scoped state that may change between model sampling requests.
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// The exact MCP config and manager used to advertise and execute tools for this step.
    pub(crate) mcp: Arc<McpRuntimeSnapshot>,
    /// The fixed MCP tool list used for this exact sampling request.
    mcp_tool_snapshot: OnceCell<Vec<ToolInfo>>,
    /// The finalized tool plan advertised and executed for this exact sampling request.
    tool_router: OnceLock<Arc<ToolRouter>>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
}

impl StepContext {
    pub(crate) fn new(
        turn: Arc<TurnContext>,
        environments: TurnEnvironmentSnapshot,
        selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
        mcp: Arc<McpRuntimeSnapshot>,
        loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    ) -> Self {
        Self {
            turn,
            environments,
            selected_capability_roots,
            mcp,
            mcp_tool_snapshot: OnceCell::new(),
            tool_router: OnceLock::new(),
            loaded_agents_md,
        }
    }

    pub(crate) async fn mcp_tools(&self) -> &[ToolInfo] {
        self.mcp_tool_snapshot
            .get_or_init(|| self.mcp.manager().list_all_tools())
            .await
    }

    pub(crate) fn set_tool_router(
        &self,
        tool_router: Arc<ToolRouter>,
    ) -> Result<(), Arc<ToolRouter>> {
        self.tool_router.set(tool_router)
    }

    pub(crate) fn tool_router(&self) -> &Arc<ToolRouter> {
        self.tool_router
            .get()
            .expect("step tool router must be finalized before tool execution")
    }

    #[cfg(test)]
    pub(crate) async fn seed_mcp_tools_for_test(&self, tools: Vec<ToolInfo>) {
        self.mcp_tool_snapshot
            .set(tools)
            .expect("test MCP tool snapshot should be unset");
    }
}
