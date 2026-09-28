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

#[derive(Clone, Debug)]
pub(crate) struct McpToolSnapshot {
    pub(crate) temporarily_unavailable: bool,
    pub(crate) revision: u64,
    pub(crate) tools: Arc<Vec<ToolInfo>>,
    pub(crate) resources_available: bool,
}

const MCP_SNAPSHOT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

async fn bounded_mcp_snapshot(
    capture: impl std::future::Future<Output = McpToolSnapshot>,
) -> McpToolSnapshot {
    match tokio::time::timeout(MCP_SNAPSHOT_DEADLINE, capture).await {
        Ok(snapshot) => snapshot,
        Err(_) => {
            tracing::warn!("MCP catalog did not stabilize before the sampling deadline");
            // Advertise no MCP capabilities when a coherent catalog is unavailable.
            // This distinct identity prevents reuse of a router from a real catalog.
            // The next step gets a new capture attempt.
            McpToolSnapshot {
                temporarily_unavailable: true,
                revision: u64::MAX,
                tools: Arc::new(Vec::new()),
                resources_available: false,
            }
        }
    }
}

/// Request-scoped state that may change between model sampling requests.
#[derive(Clone)]
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
    /// Keep the advertised capabilities but retire the previous attempt's ledger.
    pub(crate) fn for_retry_attempt(&self) -> Self {
        Self {
            workspace_evidence_generation_batch: Arc::new(WorkspaceEvidenceGenerationBatch::new()),
            ..self.clone()
        }
    }

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
            .get_or_init(|| {
                bounded_mcp_snapshot(async {
                    loop {
                        let revision = self.mcp.manager().tool_catalog_revision();
                        let (tools, resources_available) = tokio::join!(
                            self.mcp.manager().list_all_tools_snapshot(),
                            self.mcp.manager().has_ready_server_with_resources(),
                        );
                        if revision == self.mcp.manager().tool_catalog_revision() {
                            return McpToolSnapshot {
                                temporarily_unavailable: false,
                                revision,
                                tools,
                                resources_available,
                            };
                        }
                        tokio::task::yield_now().await;
                    }
                })
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
                temporarily_unavailable: false,
                revision,
                tools: Arc::new(tools),
                resources_available,
            })
            .expect("test MCP tool snapshot should be unset");
    }
}

#[cfg(test)]
mod snapshot_deadline_tests {
    use super::*;

    #[tokio::test]
    async fn retry_attempt_replaces_sealed_evidence_but_preserves_capabilities() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let step = session.capture_step_context(turn).await.unwrap();
        let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
            crate::tools::registry::ToolRegistry::from_tools(Vec::new()),
            Vec::new(),
        ));
        assert!(step.set_tool_router(Arc::clone(&router)).is_ok());
        let tracker = Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        ));
        let runtime =
            crate::tools::parallel::ToolCallRuntime::new(session, Arc::clone(&step), tracker);
        runtime.flush_workspace_evidence_generation().await.unwrap();
        let retry = step.for_retry_attempt();
        assert!(
            !step
                .workspace_evidence_generation_batch
                .register_call("late-old-call")
        );
        assert!(
            retry
                .workspace_evidence_generation_batch
                .register_call("retry-call")
        );
        assert!(Arc::ptr_eq(&step.mcp, &retry.mcp));
        assert!(Arc::ptr_eq(retry.tool_router().unwrap(), &router));
        assert!(!Arc::ptr_eq(
            &step.workspace_evidence_generation_batch,
            &retry.workspace_evidence_generation_batch
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn unstable_catalog_does_not_stall_sampling_or_advertise_partial_tools() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct CaptureGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CaptureGuard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let guard = CaptureGuard(Arc::clone(&dropped));
        let start = tokio::time::Instant::now();
        let snapshot = bounded_mcp_snapshot(async move {
            let _guard = guard;
            std::future::pending::<McpToolSnapshot>().await
        })
        .await;
        assert_eq!(start.elapsed(), MCP_SNAPSHOT_DEADLINE);
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(snapshot.tools.is_empty());
        assert!(snapshot.temporarily_unavailable);
        assert!(!snapshot.resources_available);
        assert_eq!(snapshot.revision, u64::MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn stable_catalog_keeps_its_identity_and_resource_capability() {
        let snapshot = bounded_mcp_snapshot(async {
            McpToolSnapshot {
                temporarily_unavailable: false,
                revision: 17,
                tools: Arc::new(Vec::new()),
                resources_available: true,
            }
        })
        .await;
        assert_eq!(snapshot.revision, 17);
        assert!(!snapshot.temporarily_unavailable);
        assert!(snapshot.resources_available);
    }
}
