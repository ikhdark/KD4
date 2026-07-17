use std::collections::HashMap;
use std::sync::Arc;

use crate::agents_md::LoadedAgentsMd;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::McpRuntimeSnapshot;
use crate::session::turn_context::TurnContext;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::ToolInfo;
use tokio::sync::OnceCell;

/// Request-scoped state that may change between model sampling requests.
#[derive(Debug)]
pub(crate) struct StepContext {
    pub(crate) turn: Arc<TurnContext>,
    pub(crate) environments: TurnEnvironmentSnapshot,
    /// Capability roots bound to ready environments in this exact step.
    pub(crate) selected_capability_roots: Vec<ResolvedSelectedCapabilityRoot>,
    /// The exact MCP config and manager used to advertise and execute tools for this step.
    pub(crate) mcp: Arc<McpRuntimeSnapshot>,
    /// The fixed, runtime-versioned MCP tool inventory used for this exact sampling request.
    mcp_tool_snapshot: OnceCell<McpToolInventorySnapshot>,
    /// The canonical AGENTS.md value observed with this environment snapshot.
    pub(crate) loaded_agents_md: Option<Arc<LoadedAgentsMd>>,
}

#[derive(Debug)]
pub(crate) struct McpToolInventorySnapshot {
    runtime_version: u64,
    tools: Arc<[ToolInfo]>,
    route_index: HashMap<String, HashMap<String, usize>>,
}

impl McpToolInventorySnapshot {
    pub(crate) fn new(runtime_version: u64, tools: Arc<[ToolInfo]>) -> Self {
        let mut route_index = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            route_index
                .entry(tool.server_name.clone())
                .or_insert_with(HashMap::new)
                .entry(tool.tool.name.to_string())
                .or_insert(index);
        }
        Self {
            runtime_version,
            tools,
            route_index,
        }
    }

    pub(crate) fn runtime_version(&self) -> u64 {
        self.runtime_version
    }

    pub(crate) fn tools(&self) -> &[ToolInfo] {
        self.tools.as_ref()
    }

    pub(crate) fn tool(&self, server: &str, tool_name: &str) -> Option<&ToolInfo> {
        let index = self.route_index.get(server)?.get(tool_name)?;
        self.tools.get(*index)
    }
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
            loaded_agents_md,
        }
    }

    pub(crate) async fn mcp_inventory(&self) -> &McpToolInventorySnapshot {
        self.mcp_tool_snapshot
            .get_or_init(|| async {
                let tools = Arc::<[ToolInfo]>::from(self.mcp.manager().list_all_tools().await);
                McpToolInventorySnapshot::new(self.mcp.version(), tools)
            })
            .await
    }

    pub(crate) async fn mcp_tools(&self) -> &[ToolInfo] {
        self.mcp_inventory().await.tools()
    }
}
