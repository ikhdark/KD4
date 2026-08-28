use std::sync::Arc;
use std::sync::OnceLock;

use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::RepositoryStableContextBundle;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::McpRuntimeSnapshot;
use crate::session::turn_context::TurnContext;
use crate::tools::parallel::WorkspaceEvidenceGenerationBatch;
use crate::tools::router::ToolRouter;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::ToolInfo;
#[cfg(test)]
use codex_utils_path_uri::PathUri;
use tokio::sync::OnceCell;

#[derive(Debug)]
pub(crate) struct McpToolSnapshot {
    pub(crate) revision: u64,
    pub(crate) tools: Arc<Vec<ToolInfo>>,
    pub(crate) resources_available: bool,
}

/// Request-scoped state that may change between model sampling requests.
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// The exact MCP config and manager used to advertise and execute tools for this step.
    pub(crate) mcp: Arc<McpRuntimeSnapshot>,
    /// The fixed MCP tool list used for this exact sampling request.
    mcp_tool_snapshot: OnceCell<McpToolSnapshot>,
    /// The finalized tool plan advertised and executed for this exact sampling request.
    tool_router: OnceLock<Arc<ToolRouter>>,
    /// Workspace evidence shared by every direct and nested call accepted in
    /// this exact sampling request.
    pub(crate) workspace_evidence_generation_batch: Arc<WorkspaceEvidenceGenerationBatch>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    /// The repository-instruction rendering and identity derived for this exact step.
    pub(crate) agents_md_stable_context: Option<RepositoryStableContextBundle>,
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
        let stable_context = loaded_agents_md
            .as_deref()
            .map(|loaded| loaded.stable_context_bundle(&PathUri::from_abs_path(&turn.config.cwd)));
        Self::new_with_agents_md_freshness(
            turn,
            environments,
            selected_capability_roots,
            mcp,
            loaded_agents_md,
            stable_context,
            AgentsMdFreshness::CachedFallback,
        )
    }

    pub(crate) fn new_with_agents_md_freshness(
        turn: Arc<TurnContext>,
        environments: TurnEnvironmentSnapshot,
        selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
        mcp: Arc<McpRuntimeSnapshot>,
        loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
        agents_md_stable_context: Option<RepositoryStableContextBundle>,
        agents_md_freshness: AgentsMdFreshness,
    ) -> Self {
        Self {
            turn,
            environments,
            selected_capability_roots,
            mcp,
            mcp_tool_snapshot: OnceCell::new(),
            tool_router: OnceLock::new(),
            workspace_evidence_generation_batch: Arc::new(WorkspaceEvidenceGenerationBatch::new()),
            loaded_agents_md,
            agents_md_stable_context,
            agents_md_freshness,
        }
    }

    pub(crate) async fn mcp_tool_snapshot(&self) -> &McpToolSnapshot {
        self.mcp_tool_snapshot
            .get_or_init(|| async {
                loop {
                    let revision = self.mcp.manager().tool_catalog_revision();
                    let (tools, resources_available) = tokio::join!(
                        self.mcp.manager().list_all_tools_snapshot(),
                        self.mcp.manager().has_ready_server_with_resources(),
                    );
                    if revision == self.mcp.manager().tool_catalog_revision() {
                        return McpToolSnapshot {
                            revision,
                            tools,
                            resources_available,
                        };
                    }
                }
            })
            .await
    }

    pub(crate) async fn mcp_tools(&self) -> &[ToolInfo] {
        self.mcp_tool_snapshot().await.tools.as_ref()
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
        let revision = self.mcp.manager().tool_catalog_revision();
        self.seed_mcp_tool_snapshot_for_test(revision, tools, false)
            .await;
    }

    #[cfg(test)]
    pub(crate) async fn seed_mcp_tool_snapshot_for_test(
        &self,
        revision: u64,
        tools: Vec<ToolInfo>,
        resources_available: bool,
    ) {
        self.mcp_tool_snapshot
            .set(McpToolSnapshot {
                revision,
                tools: Arc::new(tools),
                resources_available,
            })
            .expect("test MCP tool snapshot should be unset");
    }
}
