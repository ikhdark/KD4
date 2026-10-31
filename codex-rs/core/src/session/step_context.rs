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
use tokio::sync::OnceCell;

#[derive(Clone, Debug)]
pub(crate) struct McpToolSnapshot {
    pub(crate) temporarily_unavailable: bool,
    pub(crate) revision: u64,
    pub(crate) tools: Arc<Vec<ToolInfo>>,
    pub(crate) resources_available: bool,
    pub(crate) pending_servers: Vec<String>,
    pub(crate) failed_servers: Vec<String>,
}

const MCP_SNAPSHOT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

static STALE_AUTH_MCP_SNAPSHOT: std::sync::LazyLock<McpToolSnapshot> =
    std::sync::LazyLock::new(|| McpToolSnapshot {
        temporarily_unavailable: true,
        revision: u64::MAX,
        tools: Arc::new(Vec::new()),
        resources_available: false,
        pending_servers: Vec::new(),
        failed_servers: Vec::new(),
    });

async fn bounded_mcp_snapshot(
    capture: impl std::future::Future<Output = McpToolSnapshot>,
) -> McpToolSnapshot {
    match tokio::time::timeout(MCP_SNAPSHOT_DEADLINE, capture).await {
        Ok(snapshot) => snapshot,
        Err(_) => {
            tracing::warn!("MCP catalog did not stabilize before the sampling deadline");
            // Advertise no MCP capabilities when a coherent catalog is unavailable.
            // This distinct identity prevents reuse of a router from a real catalog.
            // The next sampling step gets a new capture attempt.
            McpToolSnapshot {
                temporarily_unavailable: true,
                revision: u64::MAX,
                tools: Arc::new(Vec::new()),
                resources_available: false,
                pending_servers: Vec::new(),
                failed_servers: Vec::new(),
            }
        }
    }
}

/// Request-scoped state that may change between model sampling requests.
#[derive(Clone)]
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    /// User input incorporated by the authoring request, never cell creation time.
    pub(crate) user_intent_epoch: u64,
    /// Plan owner observed before sampling, shared by direct and nested calls.
    pub(crate) plan_sampling_revision: Option<crate::plan_store::PlanSamplingRevision>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// The exact MCP config and manager used to advertise and execute tools for this step.
    pub(crate) mcp: Arc<McpRuntimeSnapshot>,
    /// Shared by accepted calls and retries of this sampling step only.
    mcp_tool_snapshot: Arc<OnceCell<McpToolSnapshot>>,
    /// One history estimate per sampling step, shared by direct and nested output owners.
    output_context_tokens: Arc<OnceCell<Option<usize>>>,
    /// The finalized tool plan advertised and executed for this exact sampling request.
    tool_router: OnceLock<Arc<ToolRouter>>,
    /// Workspace evidence shared by every direct and nested call accepted in
    /// this exact sampling request.
    pub(crate) workspace_evidence_generation_batch: Arc<WorkspaceEvidenceGenerationBatch>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
    /// The repository-instruction rendering and identity derived for this exact step.
    pub(crate) agents_md_stable_context: Option<RepositoryStableContextBundle>,
    /// Whether project instructions were completely reread for this step or use unverified fallback state.
    pub(crate) agents_md_freshness: AgentsMdFreshness,
}

impl StepContext {
    /// Keep the advertised capabilities but retire the previous attempt's ledger.
    pub(crate) fn for_retry_attempt(&self) -> Self {
        Self {
            workspace_evidence_generation_batch: Arc::new(WorkspaceEvidenceGenerationBatch::new()),
            output_context_tokens: Arc::new(OnceCell::new()),
            ..self.clone()
        }
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
        let mcp_tool_snapshot = Arc::new(OnceCell::new());
        Self {
            turn,
            user_intent_epoch: 0,
            plan_sampling_revision: None,
            environments,
            selected_capability_roots,
            mcp,
            mcp_tool_snapshot,
            output_context_tokens: Arc::new(OnceCell::new()),
            tool_router: OnceLock::new(),
            workspace_evidence_generation_batch: Arc::new(WorkspaceEvidenceGenerationBatch::new()),
            loaded_agents_md,
            agents_md_stable_context,
            agents_md_freshness,
        }
    }

    /// Bound presentation, not script-visible data processing. Keep half the
    /// remaining context for sibling outputs and the next model response; never
    /// shrink a coherent packet when the existing ceiling already fits there.
    /// This is a per-result allowance, not a reservation or a compaction gate.
    pub(crate) async fn model_output_token_limit(
        &self,
        session: &crate::session::Session,
        hard_limit: usize,
    ) -> usize {
        let available = self
            .output_context_tokens
            .get_or_init(|| async {
                let window = self.turn.model_context_window().filter(|window| *window > 0)?;
                let used = session.get_context_tokens_for_turn(&self.turn).await.max(0);
                let available =
                    usize::try_from(window.saturating_sub(used).max(0)).unwrap_or(usize::MAX) / 2;
                tracing::debug!(
                    event.name = "codex.output_context_budget",
                    context_window = window,
                    context_tokens = used,
                    output_token_limit = available,
                    "captured per-result output allowance for this sampling step"
                );
                Some(available)
            })
            .await;
        available.map_or(hard_limit, |available| hard_limit.min(available))
    }

    pub(crate) async fn mcp_tool_snapshot(&self) -> &McpToolSnapshot {
        if !self.mcp.manager().managed_auth_scope_is_current() {
            return &STALE_AUTH_MCP_SNAPSHOT;
        }
        let snapshot = self.mcp_tool_snapshot
            .get_or_init(|| async {
                bounded_mcp_snapshot(async {
                    // Required startup is validated by construction. Optional
                    // discovery must not hide another server's ready tools.
                    loop {
                        let revision = self.mcp.manager().tool_catalog_revision();
                        let (tools, resources_available, (pending_servers, failed_servers)) = tokio::join!(
                            self.mcp.manager().list_all_tools_snapshot(),
                            self.mcp.manager().has_ready_server_with_resources(),
                            self.mcp.manager().catalog_availability(),
                        );
                        if revision == self.mcp.manager().tool_catalog_revision() {
                            return McpToolSnapshot {
                                temporarily_unavailable: false,
                                revision,
                                tools,
                                resources_available,
                                pending_servers,
                                failed_servers,
                            };
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
            })
            .await;
        if !self.mcp.manager().managed_auth_scope_is_current() {
            return &STALE_AUTH_MCP_SNAPSHOT;
        }
        snapshot
    }

    pub(crate) async fn mcp_tools(&self) -> &[ToolInfo] {
        self.mcp_tool_snapshot().await.tools.as_ref()
    }

    pub(crate) fn mcp_discovery_complete(&self) -> bool {
        self.mcp.manager().managed_auth_scope_is_current()
            && self.mcp_tool_snapshot.get().is_some_and(|snapshot| {
                !snapshot.temporarily_unavailable
                    && snapshot.pending_servers.is_empty()
                    && snapshot.failed_servers.is_empty()
            })
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


}

