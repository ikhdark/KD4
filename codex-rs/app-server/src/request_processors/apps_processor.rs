use super::*;
use crate::app_info::app_info_to_api;
use crate::app_info::connector_metadata_to_api;
use codex_app_server_protocol::AppToolSummary;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::MCP_TOOL_CODEX_APPS_META_KEY;
use codex_mcp::ToolInfo;
use codex_mcp::codex_apps_tools_cache_key;
use codex_mcp::tool_is_model_visible;
use std::future::Future;
use std::pin::Pin;

mod installed;

pub(crate) struct AppsRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    workspace_settings_cache: Arc<workspace_settings::WorkspaceSettingsCache>,
    last_notified_apps: Arc<Mutex<Option<Vec<AppInfo>>>>,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl AppsRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
        workspace_settings_cache: Arc<workspace_settings::WorkspaceSettingsCache>,
        shutdown_token: CancellationToken,
    ) -> Self {
        let shutdown_drop_guard = shutdown_token.clone().drop_guard();
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            config_manager,
            workspace_settings_cache,
            last_notified_apps: Arc::new(Mutex::new(None)),
            shutdown_token,
            _shutdown_drop_guard: shutdown_drop_guard,
        }
    }

    pub(crate) async fn apps_list(
        &self,
        request_id: &ConnectionRequestId,
        params: AppsListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.apps_list_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn apps_read(
        &self,
        params: AppsReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        const APP_READ_MAX_IDS: usize = 100;

        let AppsReadParams {
            app_ids,
            include_tools,
        } = params;
        if app_ids.len() > APP_READ_MAX_IDS {
            return Err(invalid_params(format!(
                "app/read accepts at most {APP_READ_MAX_IDS} appIds"
            )));
        }

        let mut seen_app_ids = HashSet::new();
        let app_ids = app_ids
            .into_iter()
            .filter(|app_id| seen_app_ids.insert(app_id.clone()))
            .collect::<Vec<_>>();
        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        let auth = self.auth_manager.auth().await;
        if !config
            .features
            .apps_enabled_for_auth(auth.as_ref().is_some_and(CodexAuth::uses_codex_backend))
            || !workspace_codex_plugins_enabled(
                &config,
                auth.as_ref(),
                Some(&self.workspace_settings_cache),
            )
            .await
        {
            return Ok(Some(
                AppsReadResponse {
                    apps: Vec::new(),
                    missing_app_ids: app_ids,
                }
                .into(),
            ));
        }

        let loaded_plugins = self
            .thread_manager
            .plugins_manager()
            .plugins_for_config(&config.plugins_config_input())
            .await;
        let connector_snapshot =
            codex_connectors::ConnectorSnapshot::from_plugin_capability_summaries(
                loaded_plugins.capability_summaries(),
            );
        let plugin_apps = connector_snapshot.connector_ids().to_vec();
        let mut tool_summaries_by_app_id = if include_tools {
            let mcp_manager = self.thread_manager.mcp_manager();
            connectors::list_accessible_connectors_from_mcp_tools_with_mcp_manager(
                &config,
                /*force_refetch*/ false,
                self.thread_manager.environment_manager(),
                Arc::clone(&mcp_manager),
            )
            .await
            .map_err(|err| internal_error(format!("failed to read app tools: {err}")))?;
            let tools = mcp_manager
                .codex_apps_tools_cache()
                .current_tools(
                    config.codex_home.to_path_buf(),
                    codex_apps_tools_cache_key(
                        auth.as_ref(),
                        &config.chatgpt_base_url,
                        config.apps_mcp_product_sku.as_deref(),
                    ),
                )
                .unwrap_or_default();
            app_tool_summaries_by_connector(&tools)
        } else {
            HashMap::new()
        };
        let available_apps = connectors::list_all_connectors_with_options(
            &config,
            /*force_refetch*/ false,
            &plugin_apps,
        )
        .await
        .map_err(|err| internal_error(format!("failed to read app metadata: {err}")))?;
        let mut available_apps = available_apps
            .into_iter()
            .map(|app| (app.id.clone(), app))
            .collect::<HashMap<_, _>>();
        let mut apps = Vec::new();
        let mut missing_app_ids = Vec::new();
        for app_id in app_ids {
            match available_apps.remove(&app_id) {
                Some(app) => {
                    let tool_summaries = include_tools
                        .then(|| tool_summaries_by_app_id.remove(&app_id).unwrap_or_default());
                    apps.push(connector_metadata_to_api(app, tool_summaries));
                }
                None => missing_app_ids.push(app_id),
            }
        }

        Ok(Some(
            AppsReadResponse {
                apps,
                missing_app_ids,
            }
            .into(),
        ))
    }

    async fn apps_list_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: AppsListParams,
    ) -> Result<Option<AppsListResponse>, JSONRPCErrorError> {
        let installed_start = Instant::now();
        let reload = params.force_refetch;
        let thread = if let Some(thread_id) = params.thread_id.as_deref() {
            let (_, loaded_thread) = self.load_thread(thread_id).await?;
            Some(loaded_thread)
        } else {
            None
        };
        let fallback_cwd = match thread.as_ref() {
            Some(thread) => Some(thread.config_snapshot().await.cwd().to_path_buf()),
            None => None,
        };
        let mut config = self.load_latest_config(fallback_cwd).await?;

        if let Some(thread) = thread {
            let _ = config
                .features
                .set_enabled(Feature::Apps, thread.enabled(Feature::Apps));
        }

        let auth = self.auth_manager.auth().await;
        if !config
            .features
            .apps_enabled_for_auth(auth.as_ref().is_some_and(CodexAuth::uses_codex_backend))
        {
            let response = AppsListResponse {
                data: Vec::new(),
                next_cursor: None,
            };
            record_legacy_apps_installed_duration(installed_start, reload);
            return Ok(Some(response));
        }

        if !workspace_codex_plugins_enabled(
            &config,
            auth.as_ref(),
            Some(&self.workspace_settings_cache),
        )
        .await
        {
            let response = AppsListResponse {
                data: Vec::new(),
                next_cursor: None,
            };
            record_legacy_apps_installed_duration(installed_start, reload);
            return Ok(Some(response));
        }

        let request = request_id.clone();
        let outgoing = Arc::clone(&self.outgoing);
        let environment_manager = self.thread_manager.environment_manager();
        let mcp_manager = self.thread_manager.mcp_manager();
        let plugins_manager = self.thread_manager.plugins_manager();
        let last_notified_apps = Arc::clone(&self.last_notified_apps);
        let shutdown_token = self.shutdown_token.child_token();
        tokio::select! {
            _ = shutdown_token.cancelled() => {}
            _ = Self::apps_list_task(
                outgoing,
                last_notified_apps,
                request,
                params,
                config,
                environment_manager,
                mcp_manager,
                plugins_manager,
                installed_start,
            ) => {}
        }
        Ok(None)
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    #[allow(clippy::too_many_arguments)]
    async fn apps_list_task(
        outgoing: Arc<OutgoingMessageSender>,
        last_notified_apps: Arc<Mutex<Option<Vec<AppInfo>>>>,
        request_id: ConnectionRequestId,
        params: AppsListParams,
        config: Config,
        environment_manager: Arc<EnvironmentManager>,
        mcp_manager: Arc<McpManager>,
        plugins_manager: Arc<PluginsManager>,
        installed_start: Instant,
    ) {
        let reload = params.force_refetch;
        let retry_params = params.clone();
        let retry_config = config.clone();
        let retry_environment_manager = Arc::clone(&environment_manager);
        let retry_mcp_manager = Arc::clone(&mcp_manager);
        let retry_plugins_manager = Arc::clone(&plugins_manager);
        let result = Self::apps_list_response(
            &outgoing,
            &last_notified_apps,
            params,
            config,
            environment_manager,
            mcp_manager,
            plugins_manager,
        )
        .await;
        if result.is_ok() {
            record_legacy_apps_installed_duration(installed_start, reload);
        }
        let should_retry = result
            .as_ref()
            .is_ok_and(|(_, codex_apps_ready)| !codex_apps_ready);
        outgoing
            .send_result(request_id, result.map(|(response, _)| response))
            .await;

        if should_retry && !retry_params.force_refetch {
            let mut retry_params = retry_params;
            retry_params.force_refetch = true;
            if let Err(err) = Self::apps_list_response(
                &outgoing,
                &last_notified_apps,
                retry_params,
                retry_config,
                retry_environment_manager,
                retry_mcp_manager,
                retry_plugins_manager,
            )
            .await
            {
                warn!("failed to refresh app list after codex-apps readiness retry: {err:?}");
            }
        }
    }

    async fn apps_list_response(
        outgoing: &Arc<OutgoingMessageSender>,
        last_notified_apps: &Mutex<Option<Vec<AppInfo>>>,
        params: AppsListParams,
        config: Config,
        environment_manager: Arc<EnvironmentManager>,
        mcp_manager: Arc<McpManager>,
        plugins_manager: Arc<PluginsManager>,
    ) -> Result<(AppsListResponse, bool), JSONRPCErrorError> {
        let AppsListParams {
            cursor,
            limit,
            thread_id: _,
            force_refetch,
        } = params;
        let mut request_last_notified_apps = None;
        let start = match cursor {
            Some(cursor) => match cursor.parse::<usize>() {
                Ok(idx) => idx,
                Err(_) => return Err(invalid_request(format!("invalid cursor: {cursor}"))),
            },
            None => 0,
        };

        let loaded_plugins = plugins_manager
            .plugins_for_config(&config.plugins_config_input())
            .await;
        let connector_snapshot =
            codex_connectors::ConnectorSnapshot::from_plugin_capability_summaries(
                loaded_plugins.capability_summaries(),
            );
        let plugin_apps = connector_snapshot.connector_ids().to_vec();
        let (mut accessible_connectors, mut all_connectors) = tokio::join!(
            connectors::list_cached_accessible_connectors_from_mcp_tools_with_mcp_manager(
                &config,
                mcp_manager.as_ref(),
            ),
            connectors::list_cached_all_connectors(&config, &plugin_apps)
        );
        let cached_all_connectors = all_connectors.clone();

        let accessible_config = config.clone();
        let accessible_loader = async move {
            let result = connectors::list_accessible_connectors_from_mcp_tools_with_mcp_manager(
                &accessible_config,
                force_refetch,
                Arc::clone(&environment_manager),
                mcp_manager,
            )
            .await
            .map_err(|err| format!("failed to load accessible apps: {err}"));
            AppListLoadResult::Accessible(result)
        };

        let all_config = config.clone();
        let all_plugin_apps = plugin_apps.clone();
        let directory_loader = async move {
            let result = connectors::list_all_connectors_with_options(
                &all_config,
                force_refetch,
                &all_plugin_apps,
            )
            .await
            .map_err(|err| format!("failed to list apps: {err}"));
            AppListLoadResult::Directory(result)
        };
        tokio::pin!(accessible_loader);
        tokio::pin!(directory_loader);

        let app_list_deadline = tokio::time::Instant::now() + APP_LIST_LOAD_TIMEOUT;
        let mut accessible_loaded = false;
        let mut all_loaded = false;
        let mut codex_apps_ready = true;
        if accessible_connectors.is_some() || all_connectors.is_some() {
            let merged = connectors::with_app_enabled_state(
                merge_loaded_apps(all_connectors.as_deref(), accessible_connectors.as_deref()),
                &config,
            );
            if should_send_app_list_updated_notification(
                merged.as_slice(),
                accessible_loaded,
                all_loaded,
            ) {
                send_app_list_updated_notification(
                    outgoing,
                    last_notified_apps,
                    &mut request_last_notified_apps,
                    merged,
                    !force_refetch,
                )
                .await;
            }
        }

        loop {
            let result = next_app_list_load(
                accessible_loader.as_mut(),
                directory_loader.as_mut(),
                accessible_loaded,
                all_loaded,
                app_list_deadline,
            )
            .await?;

            match result {
                AppListLoadResult::Accessible(Ok(status)) => {
                    accessible_connectors = Some(status.connectors);
                    accessible_loaded = true;
                    codex_apps_ready = status.codex_apps_ready;
                }
                AppListLoadResult::Accessible(Err(err)) => {
                    return Err(internal_error(err));
                }
                AppListLoadResult::Directory(Ok(connectors)) => {
                    all_connectors = Some(connectors);
                    all_loaded = true;
                }
                AppListLoadResult::Directory(Err(err)) => {
                    return Err(internal_error(err));
                }
            }

            let showing_interim_force_refetch = force_refetch && !(accessible_loaded && all_loaded);
            let all_connectors_for_update =
                if showing_interim_force_refetch && cached_all_connectors.is_some() {
                    cached_all_connectors.as_deref()
                } else {
                    all_connectors.as_deref()
                };
            let accessible_connectors_for_update =
                if showing_interim_force_refetch && !accessible_loaded {
                    None
                } else {
                    accessible_connectors.as_deref()
                };
            let merged = connectors::with_app_enabled_state(
                merge_loaded_apps(all_connectors_for_update, accessible_connectors_for_update),
                &config,
            );
            if should_send_app_list_updated_notification(
                merged.as_slice(),
                accessible_loaded,
                all_loaded,
            ) {
                send_app_list_updated_notification(
                    outgoing,
                    last_notified_apps,
                    &mut request_last_notified_apps,
                    merged.clone(),
                    !force_refetch,
                )
                .await;
            }

            if accessible_loaded && all_loaded {
                let response = paginate_apps(merged.as_slice(), start, limit)?;
                return Ok((response, codex_apps_ready));
            }
        }
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

    async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> Result<Config, JSONRPCErrorError> {
        self.config_manager
            .load_latest_config(fallback_cwd)
            .await
            .map_config_load_error()
    }
}

async fn next_app_list_load<A, D>(
    mut accessible_loader: Pin<&mut A>,
    mut directory_loader: Pin<&mut D>,
    accessible_loaded: bool,
    all_loaded: bool,
    deadline: tokio::time::Instant,
) -> Result<AppListLoadResult, JSONRPCErrorError>
where
    A: Future<Output = AppListLoadResult>,
    D: Future<Output = AppListLoadResult>,
{
    tokio::select! {
        result = &mut accessible_loader, if !accessible_loaded => Ok(result),
        result = &mut directory_loader, if !all_loaded => Ok(result),
        _ = tokio::time::sleep_until(deadline) => {
            let timeout_seconds = APP_LIST_LOAD_TIMEOUT.as_secs();
            Err(internal_error(format!(
                "timed out waiting for app lists after {timeout_seconds} seconds"
            )))
        }
    }
}

fn app_tool_summaries_by_connector(tools: &[ToolInfo]) -> HashMap<String, Vec<AppToolSummary>> {
    let mut summaries = HashMap::<String, Vec<AppToolSummary>>::new();
    for tool in tools {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME || !tool_is_model_visible(tool) {
            continue;
        }
        if codex_connectors::connector_tool_is_synthetic(
            tool.tool
                .meta
                .as_deref()
                .and_then(|meta| meta.get(MCP_TOOL_CODEX_APPS_META_KEY)),
        ) {
            continue;
        }
        let Some(connector_id) = tool
            .connector_id
            .as_deref()
            .map(str::trim)
            .filter(|connector_id| !connector_id.is_empty())
        else {
            continue;
        };
        summaries
            .entry(connector_id.to_string())
            .or_default()
            .push(AppToolSummary {
                name: tool.tool.name.to_string(),
                title: tool.tool.title.as_deref().map(str::to_string),
                description: tool
                    .tool
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .to_string(),
            });
    }
    summaries
}

const APP_LIST_LOAD_TIMEOUT: Duration = Duration::from_secs(90);
// `app/list` is the legacy request-path baseline for the `app/installed` endpoint;
// `path=legacy` keeps it separate from the new snapshot-backed implementation in dashboards.
const APPS_INSTALLED_DURATION_METRIC: &str = "codex.apps.installed.duration_ms";

fn record_legacy_apps_installed_duration(started_at: Instant, reload: bool) {
    let reload = if reload { "true" } else { "false" };
    if let Some(metrics) = codex_otel::global() {
        let _ = metrics.record_duration(
            APPS_INSTALLED_DURATION_METRIC,
            started_at.elapsed(),
            &[("path", "legacy"), ("reload", reload)],
        );
    }
}

enum AppListLoadResult {
    Accessible(Result<AccessibleConnectorsStatus, String>),
    Directory(Result<Vec<AppInfo>, String>),
}

fn merge_loaded_apps(
    all_connectors: Option<&[AppInfo]>,
    accessible_connectors: Option<&[AppInfo]>,
) -> Vec<AppInfo> {
    let all_connectors_loaded = all_connectors.is_some();
    let all = all_connectors.map_or_else(Vec::new, <[AppInfo]>::to_vec);
    let accessible = accessible_connectors.map_or_else(Vec::new, <[AppInfo]>::to_vec);
    connectors::merge_connectors_with_accessible(all, accessible, all_connectors_loaded)
}

fn should_send_app_list_updated_notification(
    connectors: &[AppInfo],
    accessible_loaded: bool,
    all_loaded: bool,
) -> bool {
    connectors.iter().any(|connector| connector.is_accessible) || (accessible_loaded && all_loaded)
}

fn paginate_apps(
    connectors: &[AppInfo],
    start: usize,
    limit: Option<u32>,
) -> Result<AppsListResponse, JSONRPCErrorError> {
    let total = connectors.len();
    if start > total {
        return Err(invalid_request(format!(
            "cursor {start} exceeds total apps {total}"
        )));
    }

    let effective_limit = limit.unwrap_or(total as u32).max(1) as usize;
    let end = start.saturating_add(effective_limit).min(total);
    let data = connectors[start..end]
        .iter()
        .cloned()
        .map(app_info_to_api)
        .collect();
    let next_cursor = if end < total {
        Some(end.to_string())
    } else {
        None
    };

    Ok(AppsListResponse { data, next_cursor })
}

async fn send_app_list_updated_notification(
    outgoing: &Arc<OutgoingMessageSender>,
    last_notified_apps: &Mutex<Option<Vec<AppInfo>>>,
    request_last_notified_apps: &mut Option<Vec<AppInfo>>,
    data: Vec<AppInfo>,
    dedupe_across_requests: bool,
) {
    let notification_data = {
        let mut last_notified_apps = last_notified_apps.lock().await;
        if request_last_notified_apps.as_ref() == Some(&data)
            || (dedupe_across_requests && last_notified_apps.as_ref() == Some(&data))
        {
            *request_last_notified_apps = Some(data);
            return;
        }

        // Claim the snapshot atomically, but never hold the shared dedupe lock
        // while a backpressured client notification waits on the outgoing queue.
        *request_last_notified_apps = Some(data.clone());
        *last_notified_apps = Some(data.clone());
        data.into_iter().map(app_info_to_api).collect()
    };
    outgoing
        .send_server_notification(ServerNotification::AppListUpdated(
            AppListUpdatedNotification {
                data: notification_data,
            },
        ))
        .await;
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

    #[tokio::test(start_paused = true)]
    async fn app_list_timeout_drops_both_owned_loaders() {
        let accessible_dropped = Arc::new(AtomicBool::new(false));
        let directory_dropped = Arc::new(AtomicBool::new(false));

        {
            let accessible_flag = DropFlag(Arc::clone(&accessible_dropped));
            let directory_flag = DropFlag(Arc::clone(&directory_dropped));
            let accessible_loader = async move {
                let _flag = accessible_flag;
                std::future::pending::<AppListLoadResult>().await
            };
            let directory_loader = async move {
                let _flag = directory_flag;
                std::future::pending::<AppListLoadResult>().await
            };
            tokio::pin!(accessible_loader);
            tokio::pin!(directory_loader);

            let result = next_app_list_load(
                accessible_loader.as_mut(),
                directory_loader.as_mut(),
                /*accessible_loaded*/ false,
                /*all_loaded*/ false,
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await;
            assert!(result.is_err());
        }

        assert!(accessible_dropped.load(Ordering::Acquire));
        assert!(directory_dropped.load(Ordering::Acquire));
    }
}
