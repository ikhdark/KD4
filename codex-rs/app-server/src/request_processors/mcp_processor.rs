use super::*;
use std::future::Future;

async fn await_mcp_response<F, T>(
    response: F,
) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError>
where
    F: Future<Output = Result<T, JSONRPCErrorError>>,
    T: Into<ClientResponsePayload>,
{
    response.await.map(|response| Some(response.into()))
}

#[derive(Clone)]
pub(crate) struct McpRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
}

impl McpRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            config_manager,
        }
    }

    pub(crate) async fn mcp_server_oauth_login(
        &self,
        params: McpServerOauthLoginParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.mcp_server_oauth_login_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn mcp_server_refresh(
        &self,
        params: Option<()>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.mcp_server_refresh_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn mcp_server_status_list(
        &self,
        request_id: &ConnectionRequestId,
        params: ListMcpServerStatusParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        await_mcp_response(self.list_mcp_server_status(request_id, params)).await
    }

    pub(crate) async fn mcp_resource_read(
        &self,
        _request_id: &ConnectionRequestId,
        params: McpResourceReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        await_mcp_response(self.read_mcp_resource(params)).await
    }

    pub(crate) async fn mcp_server_tool_call(
        &self,
        params: McpServerToolCallParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.call_mcp_server_tool(params)
            .await
            .map(|response| Some(response.into()))
    }

    async fn mcp_server_refresh_response(
        &self,
        _params: Option<()>,
    ) -> Result<McpServerRefreshResponse, JSONRPCErrorError> {
        crate::mcp_refresh::queue_strict_refresh(&self.thread_manager, &self.config_manager)
            .await
            .map_err(|err| internal_error(format!("failed to refresh MCP servers: {err}")))?;
        Ok(McpServerRefreshResponse {})
    }

    async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> Result<Config, JSONRPCErrorError> {
        self.config_manager
            .load_latest_config(fallback_cwd)
            .await
            .map_config_load_error()
    }

    async fn load_thread(
        &self,
        thread_id: &str,
    ) -> Result<(ThreadId, Arc<CodexThread>), JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;

        Ok((thread_id, thread))
    }

    async fn mcp_server_oauth_login_response(
        &self,
        params: McpServerOauthLoginParams,
    ) -> Result<McpServerOauthLoginResponse, JSONRPCErrorError> {
        let McpServerOauthLoginParams {
            name,
            thread_id,
            scopes,
            timeout_secs,
        } = params;

        let auth = self.auth_manager.auth().await;
        let (mcp_config, runtime_context) = match thread_id.as_deref() {
            Some(thread_id) => {
                let (_, thread) = self.load_thread(thread_id).await?;
                let runtime = thread.current_mcp_runtime().await;
                (runtime.config().clone(), runtime.runtime_context().clone())
            }
            None => {
                let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
                let mcp_config = self
                    .thread_manager
                    .mcp_manager()
                    .runtime_config(&config)
                    .await;
                let runtime_context = McpRuntimeContext::new(
                    self.thread_manager.environment_manager(),
                    config.cwd.to_path_buf(),
                );
                (mcp_config, runtime_context)
            }
        };
        let effective_servers = codex_mcp::effective_mcp_servers(&mcp_config, auth.as_ref());
        let Some(server) = effective_servers
            .get(&name)
            .and_then(codex_mcp::EffectiveMcpServer::configured_config)
        else {
            return Err(invalid_request(format!(
                "No MCP server named '{name}' found."
            )));
        };

        let (url, http_headers, env_http_headers) = match &server.transport {
            McpServerTransportConfig::StreamableHttp {
                url,
                http_headers,
                env_http_headers,
                ..
            } => (url.clone(), http_headers.clone(), env_http_headers.clone()),
            _ => {
                return Err(invalid_request(
                    "OAuth login is only supported for streamable HTTP servers.",
                ));
            }
        };

        let http_client = runtime_context
            .resolve_http_client(&name, server)
            .map_err(|err| {
                internal_error(format!("failed to resolve MCP server runtime: {err}"))
            })?;

        let discovered_scopes = if scopes.is_none() && server.scopes.is_none() {
            discover_supported_scopes_with_http_client(&server.transport, Arc::clone(&http_client))
                .await
        } else {
            None
        };
        let resolved_scopes =
            resolve_oauth_scopes(scopes, server.scopes.clone(), discovered_scopes);

        let handle = perform_oauth_login_return_url_with_http_client(
            &mcp_config.codex_home,
            &name,
            &url,
            mcp_config.mcp_oauth_credentials_store_mode,
            mcp_config.auth_keyring_backend_kind,
            http_headers,
            env_http_headers,
            &resolved_scopes.scopes,
            server.oauth_client_id(),
            server.oauth_resource.as_deref(),
            timeout_secs,
            mcp_config.mcp_oauth_callback_port,
            mcp_config.mcp_oauth_callback_url.as_deref(),
            http_client,
        )
        .await
        .map_err(|err| internal_error(format!("failed to login to MCP server '{name}': {err}")))?;
        let authorization_url = handle.authorization_url().to_string();
        let notification_name = name.clone();
        let notification_thread_id = thread_id;
        let outgoing = Arc::clone(&self.outgoing);

        tokio::spawn(async move {
            let (success, error) = match handle.wait().await {
                Ok(()) => (true, None),
                Err(err) => (false, Some(err.to_string())),
            };

            let notification = ServerNotification::McpServerOauthLoginCompleted(
                McpServerOauthLoginCompletedNotification {
                    name: notification_name,
                    thread_id: notification_thread_id,
                    success,
                    error,
                },
            );
            outgoing.send_server_notification(notification).await;
        });

        Ok(McpServerOauthLoginResponse { authorization_url })
    }

    async fn list_mcp_server_status(
        &self,
        request_id: &ConnectionRequestId,
        params: ListMcpServerStatusParams,
    ) -> Result<ListMcpServerStatusResponse, JSONRPCErrorError> {
        let (config, thread) = match params.thread_id.as_deref() {
            Some(thread_id) => {
                let (_, thread) = self.load_thread(thread_id).await?;
                let thread_config = thread.config().await;
                let config = self
                    .config_manager
                    .load_latest_config_for_thread(thread_config.as_ref())
                    .await
                    .map_config_load_error()?;
                (config, Some(thread))
            }
            None => (self.load_latest_config(/*fallback_cwd*/ None).await?, None),
        };
        let mcp_manager = self.thread_manager.mcp_manager();
        let codex_apps_tools_cache = mcp_manager.codex_apps_tools_cache();
        let auth = self.auth_manager.auth().await;
        let (mcp_config, runtime_context) = match thread {
            Some(thread) => {
                let mcp_config = thread.runtime_mcp_config(&config).await;
                let runtime = thread.current_mcp_runtime().await;
                (mcp_config, runtime.runtime_context().clone())
            }
            None => {
                let mcp_config = mcp_manager.runtime_config(&config).await;
                let runtime_context = McpRuntimeContext::new(
                    self.thread_manager.environment_manager(),
                    config.cwd.to_path_buf(),
                );
                (mcp_config, runtime_context)
            }
        };

        Self::list_mcp_server_status_response(
            request_id.request_id.to_string(),
            params,
            mcp_config,
            auth,
            runtime_context,
            codex_apps_tools_cache,
        )
        .await
    }

    async fn list_mcp_server_status_response(
        request_id: String,
        params: ListMcpServerStatusParams,
        mcp_config: codex_mcp::McpConfig,
        auth: Option<CodexAuth>,
        runtime_context: McpRuntimeContext,
        codex_apps_tools_cache: codex_mcp::CodexAppsToolsCache,
    ) -> Result<ListMcpServerStatusResponse, JSONRPCErrorError> {
        let detail = match params.detail.unwrap_or(McpServerStatusDetail::Full) {
            McpServerStatusDetail::Full => McpSnapshotDetail::Full,
            McpServerStatusDetail::ToolsAndAuthOnly => McpSnapshotDetail::ToolsAndAuthOnly,
        };
        let mut server_names = codex_mcp::effective_mcp_servers(&mcp_config, auth.as_ref())
            .into_keys()
            .collect::<Vec<_>>();
        server_names.sort();

        let total = server_names.len();
        let limit = params.limit.unwrap_or(total as u32).max(1) as usize;
        let effective_limit = limit.min(total);
        let start = match params.cursor {
            Some(cursor) => match cursor.parse::<usize>() {
                Ok(idx) => idx,
                Err(_) => return Err(invalid_request(format!("invalid cursor: {cursor}"))),
            },
            None => 0,
        };

        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total MCP servers {total}"
            )));
        }

        let end = start.saturating_add(effective_limit).min(total);
        let selected_server_names = server_names[start..end].to_vec();

        let snapshot = collect_mcp_server_status_snapshot_for_servers_with_detail(
            &mcp_config,
            auth.as_ref(),
            request_id,
            runtime_context,
            codex_apps_tools_cache,
            detail,
            &selected_server_names,
        )
        .await;

        let McpServerStatusSnapshot {
            server_infos,
            tools_by_server,
            resources,
            resource_templates,
            auth_statuses,
            server_names: _,
        } = snapshot;

        let data: Vec<McpServerStatus> = selected_server_names
            .iter()
            .map(|name| McpServerStatus {
                name: name.clone(),
                server_info: server_infos.get(name).cloned(),
                tools: tools_by_server.get(name).cloned().unwrap_or_default(),
                resources: resources.get(name).cloned().unwrap_or_default(),
                resource_templates: resource_templates.get(name).cloned().unwrap_or_default(),
                auth_status: auth_statuses
                    .get(name)
                    .cloned()
                    .unwrap_or(CoreMcpAuthStatus::Unsupported)
                    .into(),
            })
            .collect();

        let next_cursor = if end < total {
            Some(end.to_string())
        } else {
            None
        };

        Ok(ListMcpServerStatusResponse { data, next_cursor })
    }

    async fn read_mcp_resource(
        &self,
        params: McpResourceReadParams,
    ) -> Result<McpResourceReadResponse, JSONRPCErrorError> {
        let McpResourceReadParams {
            thread_id,
            server,
            uri,
        } = params;

        if let Some(thread_id) = thread_id {
            let (_, thread) = self.load_thread(&thread_id).await?;
            let result = thread.read_mcp_resource(&server, &uri).await;
            return Self::mcp_resource_read_response(result);
        }

        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        let mcp_manager = self.thread_manager.mcp_manager();
        let mcp_config = mcp_manager.runtime_config(&config).await;
        let codex_apps_tools_cache = mcp_manager.codex_apps_tools_cache();
        let auth = self.auth_manager.auth().await;
        let environment_manager = self.thread_manager.environment_manager();
        // This threadless resource-read path has no turn cwd or turn-selected
        // environment. Use config cwd only as the local stdio fallback; named
        // environment stdio MCPs must declare their own absolute cwd.
        let runtime_context =
            McpRuntimeContext::new(Arc::clone(&environment_manager), config.cwd.to_path_buf());

        let result = read_mcp_resource_without_thread(
            &mcp_config,
            auth.as_ref(),
            runtime_context,
            codex_apps_tools_cache,
            &server,
            &uri,
        )
        .await
        .and_then(|result| serde_json::to_value(result).map_err(anyhow::Error::from));
        Self::mcp_resource_read_response(result)
    }

    fn mcp_resource_read_response(
        result: anyhow::Result<serde_json::Value>,
    ) -> Result<McpResourceReadResponse, JSONRPCErrorError> {
        result
            .map_err(|error| internal_error(format!("{error:#}")))
            .and_then(|result| {
                serde_json::from_value::<McpResourceReadResponse>(result).map_err(|error| {
                    internal_error(format!(
                        "failed to deserialize MCP resource read response: {error}"
                    ))
                })
            })
    }

    async fn call_mcp_server_tool(
        &self,
        params: McpServerToolCallParams,
    ) -> Result<McpServerToolCallResponse, JSONRPCErrorError> {
        let thread_id = params.thread_id.clone();
        let (_, thread) = self.load_thread(&thread_id).await?;
        let meta = codex_protocol::mcp::with_tool_call_thread_id_meta(params.meta, &thread_id);

        thread
            .call_mcp_tool(&params.server, &params.tool, params.arguments, meta)
            .await
            .map(McpServerToolCallResponse::from)
            .map_err(|error| internal_error(format!("{error:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn gate_owned_mcp_response_drops_semantic_work_when_cancelled() {
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));

        {
            let work_started = Arc::clone(&started);
            let work_dropped = DropFlag(Arc::clone(&dropped));
            let response = await_mcp_response(async move {
                let _work_dropped = work_dropped;
                work_started.notify_one();
                std::future::pending::<Result<ListMcpServerStatusResponse, JSONRPCErrorError>>()
                    .await
            });
            tokio::pin!(response);

            tokio::select! {
                result = &mut response => panic!("semantic MCP work completed unexpectedly: {result:?}"),
                _ = started.notified() => {}
            }
        }

        assert!(dropped.load(Ordering::Acquire));
    }
}
