use std::sync::Arc;
use std::sync::OnceLock;

use crate::agents_md::AgentsMdFreshness;
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
    /// Whether that value came from this step's read or a fallback cache.
    pub(crate) agents_md_freshness: AgentsMdFreshness,
}

impl StepContext {
    #[cfg(test)]
    pub(crate) fn new(
        turn: Arc<TurnContext>,
        environments: TurnEnvironmentSnapshot,
        selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
        mcp: Arc<McpRuntimeSnapshot>,
        loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    ) -> Self {
        Self::new_with_agents_md_freshness(
            turn,
            environments,
            selected_capability_roots,
            mcp,
            loaded_agents_md,
            AgentsMdFreshness::CachedFallback,
        )
    }

    pub(crate) fn new_with_agents_md_freshness(
        turn: Arc<TurnContext>,
        environments: TurnEnvironmentSnapshot,
        selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
        mcp: Arc<McpRuntimeSnapshot>,
        loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
        agents_md_freshness: AgentsMdFreshness,
    ) -> Self {
        Self {
            turn,
            environments,
            selected_capability_roots,
            mcp,
            mcp_tool_snapshot: OnceCell::new(),
            tool_router: OnceLock::new(),
            loaded_agents_md,
            agents_md_freshness,
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

    pub(crate) fn tool_router(&self) -> Option<&Arc<ToolRouter>> {
        self.tool_router.get()
    }

    #[cfg(test)]
    pub(crate) async fn seed_mcp_tools_for_test(&self, tools: Vec<ToolInfo>) {
        self.mcp_tool_snapshot
            .set(tools)
            .expect("test MCP tool snapshot should be unset");
    }
}
