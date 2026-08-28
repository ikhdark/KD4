use std::fmt;
use std::sync::Arc;

use codex_mcp::McpConfig;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpRuntimeContext;

pub(crate) struct McpManagerLifecycle {
    manager: Arc<McpConnectionManager>,
}

impl McpManagerLifecycle {
    fn new(manager: Arc<McpConnectionManager>) -> Self {
        Self { manager }
    }
}

impl Drop for McpManagerLifecycle {
    fn drop(&mut self) {
        let manager = Arc::clone(&self.manager);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn new_uninitialized_for_test(config: &crate::config::Config) -> Arc<Self> {
        use codex_exec_server::EnvironmentManager;
        use codex_features::Feature;
        use codex_mcp::ResolvedMcpCatalog;
        use rmcp::model::ElicitationCapability;

        let mcp_config = McpConfig {
            chatgpt_base_url: config.chatgpt_base_url.clone(),
            apps_mcp_product_sku: config.apps_mcp_product_sku.clone(),
            codex_home: config.codex_home.to_path_buf(),
            mcp_oauth_credentials_store_mode: config.mcp_oauth_credentials_store_mode,
            auth_keyring_backend_kind: config.auth_keyring_backend_kind(),
            mcp_oauth_callback_port: config.mcp_oauth_callback_port,
            mcp_oauth_callback_url: config.mcp_oauth_callback_url.clone(),
            skill_mcp_dependency_install_enabled: config
                .features
                .enabled(Feature::SkillMcpDependencyInstall),
            approval_policy: config.permissions.approval_policy.clone(),
            apps_enabled: config.features.enabled(Feature::Apps),
            prefix_mcp_tool_names: config.prefix_mcp_tool_names(),
            client_elicitation_capability: ElicitationCapability::default(),
            mcp_server_catalog: ResolvedMcpCatalog::default(),
            connector_snapshot: codex_connectors::ConnectorSnapshot::default(),
        };
        let manager = McpConnectionManager::new_uninitialized_with_permission_profile(
            &config.permissions.approval_policy,
            config.permissions.permission_profile(),
            config.prefix_mcp_tool_names(),
        );
        let runtime_context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            config.cwd.to_path_buf(),
        );
        Arc::new(Self::new(
            0,
            Arc::new(mcp_config),
            /*plugins_available*/ false,
            Arc::new(manager),
            runtime_context,
            Vec::new(),
        ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::Constrained;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::protocol::AskForApproval;
    use std::time::Duration;

    #[tokio::test]
    async fn final_runtime_lifecycle_owner_starts_manager_shutdown() {
        let approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
        let manager = Arc::new(
            McpConnectionManager::new_uninitialized_with_permission_profile(
                &approval_policy,
                &PermissionProfile::default(),
                /*prefix_mcp_tool_names*/ true,
            ),
        );
        let first = Arc::new(McpManagerLifecycle::new(Arc::clone(&manager)));
        let second = Arc::clone(&first);

        drop(first);
        assert!(!manager.shutdown_started());
        drop(second);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.shutdown_started() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("final runtime lifecycle owner should start manager shutdown");
    }
}
