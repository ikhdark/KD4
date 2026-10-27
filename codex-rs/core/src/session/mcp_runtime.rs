use std::fmt;
use std::sync::Arc;

use codex_mcp::McpConfig;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpRuntimeContext;

pub(crate) struct McpManagerLifecycle {
    manager: Arc<McpConnectionManager>,
    shutdown_runtime: Option<tokio::runtime::Handle>,
}

impl McpManagerLifecycle {
    fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self {
            manager,
            // A final snapshot lease may be released outside an entered runtime.
            shutdown_runtime: tokio::runtime::Handle::try_current().ok(),
        }
    }
}

impl Drop for McpManagerLifecycle {
    fn drop(&mut self) {
        let manager = Arc::clone(&self.manager);
        let Some(runtime) = self
            .shutdown_runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        runtime.spawn(async move {
            manager.shutdown().await;
        });
    }
}

/// MCP config, plugin availability, exact environment bindings, and manager for one request.
pub struct McpRuntimeSnapshot {
    generation: u64,
    config: Arc<McpConfig>,
    plugins_available: bool,
    manager: Arc<McpConnectionManager>,
    manager_lifecycle: Arc<McpManagerLifecycle>,
    runtime_context: McpRuntimeContext,
    available_environment_ids: Vec<String>,
}

impl McpRuntimeSnapshot {
    pub(crate) fn new(
        generation: u64,
        config: Arc<McpConfig>,
        plugins_available: bool,
        manager: Arc<McpConnectionManager>,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
    ) -> Self {
        let manager_lifecycle = Arc::new(McpManagerLifecycle::new(Arc::clone(&manager)));
        Self::new_with_manager_lifecycle(
            generation,
            config,
            plugins_available,
            manager,
            manager_lifecycle,
            runtime_context,
            available_environment_ids,
        )
    }

    pub(crate) fn new_with_manager_lifecycle(
        generation: u64,
        config: Arc<McpConfig>,
        plugins_available: bool,
        manager: Arc<McpConnectionManager>,
        manager_lifecycle: Arc<McpManagerLifecycle>,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
    ) -> Self {
        debug_assert!(Arc::ptr_eq(&manager, &manager_lifecycle.manager));
        Self {
            generation,
            config,
            plugins_available,
            manager,
            manager_lifecycle,
            runtime_context,
            available_environment_ids,
        }
    }


    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub fn config(&self) -> &McpConfig {
        self.config.as_ref()
    }

    pub(crate) fn plugins_available(&self) -> bool {
        self.plugins_available
    }

    pub fn manager(&self) -> &McpConnectionManager {
        self.manager.as_ref()
    }

    pub(crate) fn manager_arc(&self) -> Arc<McpConnectionManager> {
        Arc::clone(&self.manager)
    }

    pub(crate) fn manager_lifecycle_arc(&self) -> Arc<McpManagerLifecycle> {
        Arc::clone(&self.manager_lifecycle)
    }

    pub fn runtime_context(&self) -> &McpRuntimeContext {
        &self.runtime_context
    }

    pub(crate) fn available_environment_ids(&self) -> &[String] {
        &self.available_environment_ids
    }


}

impl fmt::Debug for McpRuntimeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpRuntimeSnapshot")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

