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
    use crate::session::tests::make_session_and_context_with_rx;
    use codex_config::types::McpServerConfig;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    enum Finalization {
        Lease { outside_runtime: bool },
        Session { retain_step: bool },
    }

    #[tokio::test]
    async fn final_runtime_lifecycle_owner_closes_registered_transport() -> anyhow::Result<()> {
        assert_final_runtime_lifecycle_owner_closes_registered_transport(Finalization::Lease {
            outside_runtime: false,
        })
        .await
    }

    #[tokio::test]
    async fn final_runtime_lifecycle_owner_closes_registered_transport_outside_entered_runtime()
    -> anyhow::Result<()> {
        assert_final_runtime_lifecycle_owner_closes_registered_transport(Finalization::Lease {
            outside_runtime: true,
        })
        .await
    }

    #[tokio::test]
    async fn session_shutdown_closes_retired_manager_with_live_step_lease() -> anyhow::Result<()> {
        assert_final_runtime_lifecycle_owner_closes_registered_transport(Finalization::Session {
            retain_step: true,
        })
        .await
    }

    #[tokio::test]
    async fn session_shutdown_waits_for_already_running_retired_manager_cleanup()
    -> anyhow::Result<()> {
        assert_final_runtime_lifecycle_owner_closes_registered_transport(Finalization::Session {
            retain_step: false,
        })
        .await
    }

    async fn assert_final_runtime_lifecycle_owner_closes_registered_transport(
        finalization: Finalization,
    ) -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let result = match body["method"].as_str().unwrap() {
                    "initialize" => json!({
                        "protocolVersion": body["params"]["protocolVersion"],
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "lifecycle-server", "version": "1"},
                    }),
                    "notifications/initialized" => return ResponseTemplate::new(202),
                    "tools/list" => json!({"tools": [{
                        "name": "echo",
                        "inputSchema": {"type": "object", "properties": {}},
                    }]}),
                    "tools/call" => json!({
                        "content": [{"type": "text", "text": body["params"]["arguments"]["message"]}],
                        "isError": false,
                    }),
                    _ => return ResponseTemplate::new(400),
                };
                ResponseTemplate::new(200)
                    .insert_header("mcp-session-id", "retired-runtime-session")
                    .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}))
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        let deleted = Arc::new(tokio::sync::Notify::new());
        let delete_delay = if matches!(finalization, Finalization::Session { .. }) {
            Duration::from_millis(500)
        } else {
            Duration::ZERO
        };
        Mock::given(method("DELETE"))
            .and(path("/mcp"))
            .respond_with({
                let deleted = Arc::clone(&deleted);
                move |_request: &Request| {
                    deleted.notify_one();
                    ResponseTemplate::new(204).set_delay(delete_delay)
                }
            })
            .expect(1)
            .mount(&server)
            .await;

        let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
        let mut configured = turn_context.config.as_ref().clone();
        configured
            .mcp_servers
            .set(serde_json::from_value::<HashMap<String, McpServerConfig>>(
                json!({"lifecycle": {"url": format!("{}/mcp", server.uri())}}),
            )?)?;
        session
            .refresh_mcp_servers_now(&turn_context, &configured, None)
            .await;
        let old_runtime = session.services.latest_mcp_runtime();
        // A raw manager observer does not own a runtime lease. Keeping it alive
        // ensures McpConnectionManager::drop cannot mask a missing lifecycle close.
        let old_manager = old_runtime.manager_arc();
        let tools =
            tokio::time::timeout(Duration::from_secs(5), old_manager.list_all_tools()).await?;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool.name.as_ref(), "echo");
        let step = session
            .capture_step_context(Arc::clone(&turn_context))
            .await;
        assert!(Arc::ptr_eq(&step.mcp, &old_runtime));

        let mut removed = configured.clone();
        removed.mcp_servers.set(HashMap::new())?;
        session
            .refresh_mcp_servers_now(&turn_context, &removed, None)
            .await;
        let replacement = session.services.latest_mcp_runtime();
        assert!(!Arc::ptr_eq(&old_runtime, &replacement));
        assert!(replacement.manager().list_all_tools().await.is_empty());
        let mut old_runtime = Some(old_runtime);
        for message in [
            "snapshot and step retain the connection",
            "only the step retains the connection",
        ] {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                step.mcp.manager().call_tool(
                    "lifecycle",
                    "echo",
                    Some(json!({"message": message})),
                    None,
                ),
            )
            .await??;
            assert_eq!(result.is_error, Some(false));
            assert_eq!(
                result.content,
                vec![json!({"type": "text", "text": message})]
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(50), deleted.notified())
                    .await
                    .is_err(),
                "refresh and non-final lease release must not close a connection still used by a step"
            );
            drop(old_runtime.take());
        }
        match finalization {
            Finalization::Lease { outside_runtime } => {
                if outside_runtime {
                    std::thread::spawn(move || {
                        assert!(tokio::runtime::Handle::try_current().is_err());
                        drop(step);
                    })
                    .join()
                    .expect("final lifecycle lease drops outside an entered runtime");
                } else {
                    drop(step);
                }
                tokio::time::timeout(Duration::from_secs(5), deleted.notified()).await?;
            }
            Finalization::Session { retain_step } => {
                let mut step = Some(step);
                if !retain_step {
                    let first_shutdown = tokio::spawn({
                        let manager = Arc::clone(&old_manager);
                        async move { manager.shutdown().await }
                    });
                    tokio::time::timeout(Duration::from_secs(5), deleted.notified()).await?;
                    first_shutdown.abort();
                    assert!(first_shutdown.await.unwrap_err().is_cancelled());
                    drop(step.take());
                }
                let shutdown = tokio::spawn({
                    let session = Arc::clone(&session);
                    async move {
                        crate::session::handlers::shutdown(&session, "mcp-shutdown".to_string())
                            .await
                    }
                });
                if retain_step {
                    tokio::time::timeout(Duration::from_secs(5), deleted.notified()).await?;
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), async {
                        loop {
                            if matches!(
                                rx_event.recv().await.unwrap().msg,
                                codex_protocol::protocol::EventMsg::ShutdownComplete
                            ) {
                                break;
                            }
                        }
                    })
                    .await
                    .is_err(),
                    "ShutdownComplete must wait for the retired transport DELETE response"
                );
                assert!(tokio::time::timeout(Duration::from_secs(5), shutdown).await??);
                assert!(old_manager.shutdown_finished());
                assert!(replacement.manager().shutdown_finished());
                assert!(matches!(
                    rx_event.recv().await?.msg,
                    codex_protocol::protocol::EventMsg::ShutdownComplete
                ));
                // A live caller lease cannot delay session shutdown indefinitely.
                drop(step);
            }
        }
        let requests = server
            .received_requests()
            .await
            .expect("recorded server requests");
        let deletions = requests
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .collect::<Vec<_>>();
        assert_eq!(deletions.len(), 1);
        assert_eq!(
            deletions[0]
                .headers
                .get("mcp-session-id")
                .unwrap()
                .to_str()?,
            "retired-runtime-session"
        );
        let calls_before = requests
            .iter()
            .filter(|request| {
                serde_json::from_slice::<serde_json::Value>(&request.body)
                    .is_ok_and(|body| body["method"] == "tools/call")
            })
            .count();
        assert_eq!(calls_before, 2);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                old_manager.call_tool(
                    "lifecycle",
                    "echo",
                    Some(json!({"message": "must never reach the server"})),
                    None,
                )
            )
            .await?
            .is_err(),
            "a shut down client must reject further calls"
        );
        let calls_after = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                serde_json::from_slice::<serde_json::Value>(&request.body)
                    .is_ok_and(|body| body["method"] == "tools/call")
            })
            .count();
        assert_eq!(
            calls_after, calls_before,
            "closed transport must not send another tool request"
        );
        Ok(())
    }
}
