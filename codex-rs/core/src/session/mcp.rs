use super::*;
use crate::mcp::McpRuntimeProjection;
use crate::turn_timing::InteractiveWaitKind;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_KEY as MCP_ELICITATION_APPROVAL_KIND_KEY;
use codex_protocol::mcp_approval_meta::APPROVAL_KIND_TOOL_SUGGESTION as MCP_ELICITATION_APPROVAL_KIND_TOOL_SUGGESTION;
use codex_protocol::mcp_approval_meta::TOOL_NAME_KEY as MCP_ELICITATION_TOOL_NAME_KEY;
use serde_json::Map;

const TOOL_SUGGESTION_ACTION_INSTALL: &str = "install";
const TOOL_SUGGESTION_ACTION_KEY: &str = "suggest_type";
const TOOL_SUGGESTION_TOOL_ID_KEY: &str = "tool_id";
const TOOL_SUGGESTION_TOOL_TYPE_KEY: &str = "tool_type";





struct McpStartupCancellationGuard {
    token: CancellationToken,
    published: bool,
}

impl McpStartupCancellationGuard {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            published: false,
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for McpStartupCancellationGuard {
    fn drop(&mut self) {
        if !self.published {
            self.token.cancel();
        }
    }
}

pub(crate) struct McpServerElicitationOutcome {
    pub(crate) response: Option<ElicitationResponse>,
    pub(crate) sent: bool,
}

/// Only the immutable request authority consumed by MCP projection/startup.
/// Ordinary turns borrow it; out-of-band calls need no synthetic TurnContext.
struct McpRequestBinding<'a> {
    config: &'a Config,
    originator: &'a str,
    sub_id: &'a str,
    approval_policy: &'a codex_config::Constrained<AskForApproval>,
    permission_profile: &'a codex_protocol::models::PermissionProfile,
}

impl<'a> McpRequestBinding<'a> {
    fn from_turn(turn: &'a TurnContext) -> Self {
        Self {
            config: &turn.config,
            originator: &turn.originator,
            sub_id: &turn.sub_id,
            approval_policy: &turn.approval_policy,
            permission_profile: &turn.permission_profile,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PluginInstallElicitationTelemetryMetadata {
    tool_type: String,
    tool_id: String,
    tool_name: String,
}

impl Session {
    pub(crate) async fn lock_pending_mcp_server_refresh_config(
        &self,
    ) -> tokio::sync::MutexGuard<'_, Option<Arc<McpServerRefreshConfig>>> {
        self.pending_mcp_server_refresh_config.lock().await
    }

    pub(crate) async fn runtime_mcp_config(&self, config: &Config) -> McpConfig {
        let originator = self.originator().await;
        let environments = self.services.turn_environments.snapshot().await;
        let selected_capability_roots = self
            .resolve_selected_capability_roots_for_step(&environments)
            .await;
        let available_environment_ids =
            Self::available_selected_environment_ids(&selected_capability_roots);
        self.services
            .mcp_manager
            .runtime_config_for_bound_step(
                config,
                &self.services.mcp_thread_init,
                &self.services.thread_extension_data,
                &originator,
                &available_environment_ids,
                &selected_capability_roots,
            )
            .await
            .config
    }

    pub(crate) async fn runtime_mcp_servers(
        &self,
        config: &Config,
    ) -> HashMap<String, McpServerConfig> {
        codex_mcp::configured_mcp_servers(&self.runtime_mcp_config(config).await)
    }

    pub(crate) async fn mcp_runtime_for_step(
        self: &Arc<Self>,
        turn_context: &TurnContext,
        environments: &TurnEnvironmentSnapshot,
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> codex_protocol::error::Result<Arc<McpRuntimeSnapshot>> {
        self.mcp_runtime_for_binding(
            &McpRequestBinding::from_turn(turn_context), environments, selected_capability_roots,
        ).await
    }

    pub(crate) async fn capture_current_mcp_runtime(
        self: &Arc<Self>,
    ) -> codex_protocol::error::Result<Arc<McpRuntimeSnapshot>> {
        let environments = self.services.turn_environments.snapshot().await;
        let roots = self.resolve_selected_capability_roots_for_step(&environments).await;
        let available = Self::available_selected_environment_ids(&roots);
        let context = self.bound_mcp_context(&environments, &roots);
        let current = self.services.latest_mcp_runtime();
        if current.available_environment_ids() == available
            && current.runtime_context().selected_scope_matches(&context)
        {
            return Ok(current);
        }
        let configuration = self.state.lock().await.session_configuration.clone();
        let cwd = environments.primary()
            .and_then(|environment| environment.cwd().to_abs_path().ok())
            .unwrap_or_else(|| configuration.cwd().clone());
        let mut config = Self::build_per_turn_config(&configuration, cwd);
        // Contributors receive the effective Config and thread ModelInfo. Keep
        // those inputs exact on a rebuild, without discovering skills/catalogs,
        // refreshing instructions/plan state, or spawning Git enrichment.
        let model = self.services.models_manager.get_model_info(
            configuration.collaboration_mode.model(), &config.to_models_manager_config(),
        ).await;
        self.services.thread_extension_data.insert(model.clone());
        self.resolve_multi_agent_version_for_model(&model, &config);
        let tier = get_service_tier(
            config.service_tier.clone(), config.features.enabled(Feature::FastMode), &model,
        );
        if config.service_tier != tier {
            Arc::make_mut(&mut config).service_tier = tier;
        }
        let permission_profile = configuration.permission_profile();
        let sub_id = self.next_internal_sub_id();
        let binding = McpRequestBinding {
            config: &config,
            originator: &configuration.originator,
            sub_id: &sub_id,
            approval_policy: &configuration.approval_policy,
            permission_profile: &permission_profile,
        };
        self.mcp_runtime_for_binding(&binding, &environments, &roots).await
    }

    fn bound_mcp_context(
        &self,
        environments: &TurnEnvironmentSnapshot,
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> McpRuntimeContext {
        let current_context = self.services.latest_mcp_runtime().runtime_context().clone();
        let local_cwd = environments
            .turn_environments
            .iter()
            .find(|environment| !environment.environment.is_remote())
            .and_then(|environment| environment.cwd().to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| current_context.local_fallback_cwd());
        let mut captured = environments.captured_environments();
        for root in selected_capability_roots {
            let CapabilityRootLocation::Environment { environment_id, .. } =
                &root.selected_root().location;
            captured.insert(environment_id.clone(), Some(Arc::clone(root.environment())));
        }
        McpRuntimeContext::new(
            self.services.turn_environments.environment_manager(),
            local_cwd,
        )
        .with_selected_environments(captured)
    }

    async fn mcp_runtime_for_binding(
        self: &Arc<Self>,
        binding: &McpRequestBinding<'_>,
        environments: &TurnEnvironmentSnapshot,
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> codex_protocol::error::Result<Arc<McpRuntimeSnapshot>> {
        let available_environment_ids = Self::available_selected_environment_ids(selected_capability_roots);
        let selected_context = self.bound_mcp_context(environments, selected_capability_roots);
        loop {
            let expected_runtime = self.services.latest_mcp_runtime();
            let scope_matches = |runtime: &McpRuntimeSnapshot| {
                runtime
                    .config()
                    .mcp_server_catalog
                    .configured_servers()
                    .values()
                    .filter(|server| server.enabled)
                    .all(|server| {
                        runtime
                            .runtime_context()
                            .server_scope_matches(&selected_context, server)
                    })
            };
            if expected_runtime.available_environment_ids() == available_environment_ids
                && expected_runtime
                    .runtime_context()
                    .selected_scope_matches(&selected_context)
            {
                return Ok(expected_runtime);
            }
            let mcp_projection = self
                .services
                .mcp_manager
                .runtime_config_for_bound_step(
                    binding.config,
                    &self.services.mcp_thread_init,
                    &self.services.thread_extension_data,
                    binding.originator,
                    &available_environment_ids,
                    selected_capability_roots,
                )
                .await;
            let expected_runtime = {
                let Ok(_guard) = self.services.mcp_projection_lock.acquire().await else {
                    unreachable!("MCP projection semaphore is never closed");
                };
                let current = self.services.latest_mcp_runtime();
                if current.available_environment_ids() == available_environment_ids
                    && current
                        .runtime_context()
                        .selected_scope_matches(&selected_context)
                {
                    return Ok(current);
                }
                if !Arc::ptr_eq(&current, &expected_runtime) {
                    continue;
                }
                let mcp_config = &mcp_projection.config;
                let changed_environment_is_used_by_mcp = mcp_config
                    .mcp_server_catalog
                    .configured_servers()
                    .values()
                    .any(|server| {
                        let was_available = current
                            .available_environment_ids()
                            .contains(&server.environment_id);
                        let is_available =
                            available_environment_ids.contains(&server.environment_id);
                        server.enabled && was_available != is_available
                    });
                if !changed_environment_is_used_by_mcp
                    && scope_matches(&current)
                    && current
                        .config()
                        .mcp_server_catalog
                        .has_same_servers(&mcp_config.mcp_server_catalog)
                    && current.config().connector_snapshot == mcp_config.connector_snapshot
                {
                    // Availability is only an input to the MCP projection. When that input changes
                    // but the projected servers and connectors do not, advance the input key without
                    // replacing the live manager and restarting its processes.
                    let mut state_owner = self.state.lock().await;
                    return Ok(self.services.publish_mcp_runtime_reusing_manager(
                        &mut state_owner,
                        Arc::new(current.config().clone()),
                        mcp_projection.plugins_available,
                        selected_context.clone(),
                        available_environment_ids,
                        current.as_ref(),
                    ));
                }
                current
            };
            // Cancellation only blocks a refresh that must start a new manager.
            // Environment-only changes above can still reuse a coherent manager.
            if self
                .services
                .mcp_startup_cancellation_token
                .lock()
                .await
                .is_cancelled()
            {
                return Err(codex_protocol::error::CodexErr::TurnAborted);
            }
            let runtime = self
                .refresh_mcp_servers_inner(
                    binding,
                    mcp_projection,
                    environments,
                    &available_environment_ids,
                    expected_runtime,
                    Some(selected_context.clone()),
                )
                .await;
            if runtime.available_environment_ids() == available_environment_ids
                && runtime
                    .runtime_context()
                    .selected_scope_matches(&selected_context)
            {
                return Ok(runtime);
            }
            // A concurrent refresh published a projection for a different environment while this
            // candidate was starting. Rebuild from that publication instead of returning an MCP
            // runtime that does not match the step's resolved capability roots.
        }
    }

    pub(crate) async fn resolve_selected_capability_roots_for_step(
        &self,
        environments: &TurnEnvironmentSnapshot,
    ) -> Vec<ResolvedSelectedCapabilityRoot> {
        self.services
            .turn_environments
            .environment_manager()
            .resolve_selected_capability_roots(
                &self.services.selected_capability_roots,
                &environments.captured_environments(),
            )
            .await
    }

    pub(crate) fn mcp_elicitation_lifecycle(&self) -> codex_mcp::ElicitationLifecycle {
        let elicitations = self.services.elicitations.clone();
        codex_mcp::ElicitationLifecycle::new(move || elicitations.register())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_mcp_server_elicitation(
        &self,
        turn_context: &TurnContext,
        server_name: String,
        request_id: RequestId,
        request: ElicitationRequest,
    ) -> McpServerElicitationOutcome {
        if self
            .services
            .latest_mcp_runtime()
            .manager()
            .elicitations_auto_deny()
        {
            return McpServerElicitationOutcome {
                response: Some(ElicitationResponse {
                    action: codex_rmcp_client::ElicitationAction::Accept,
                    content: Some(serde_json::json!({})),
                    meta: None,
                }),
                sent: false,
            };
        }

        let cleanup = tokio_util::sync::CancellationToken::new();
        let _cleanup_on_drop = cleanup.clone().drop_guard();
        let (tx_response, rx_response) = oneshot::channel();
        let (originating_turn_state, prev_entry, _elicitation) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut().filter(|at| {
                // Terminal claim removes the task under this same lock. A
                // taskless startup reservation cannot own an interaction.
                at.task.as_ref().is_some_and(|task| {
                    task.turn_context.sub_id == turn_context.sub_id
                        && !task.cancellation_token.is_cancelled()
                }) && at
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.turn_id() == turn_context.sub_id)
            }) {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    let previous = ts.insert_pending_elicitation(
                        server_name.clone(),
                        request_id.clone(),
                        tx_response,
                    );
                    (
                        Arc::clone(&at.turn_state),
                        previous,
                        self.services.elicitations.register(),
                    )
                }
                None => {
                    return McpServerElicitationOutcome {
                        response: None,
                        sent: false,
                    };
                }
            }
        };
        {
            let turn_state = originating_turn_state;
            let server_name = server_name.clone();
            let request_id = request_id.clone();
            self.terminal_tasks.spawn(async move {
                cleanup.cancelled().await;
                turn_state
                    .lock()
                    .await
                    .remove_closed_pending_elicitation(&server_name, &request_id);
            });
        }
        if prev_entry.is_some() {
            warn!(
                "Overwriting existing pending elicitation for server_name: {server_name}, request_id: {request_id}"
            );
        }
        drop(prev_entry);
        let id = match request_id {
            rmcp::model::NumberOrString::String(value) => {
                codex_protocol::mcp::RequestId::String(value.to_string())
            }
            rmcp::model::NumberOrString::Number(value) => {
                codex_protocol::mcp::RequestId::Integer(value)
            }
        };
        let event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
            turn_id: Some(turn_context.sub_id.clone()),
            server_name,
            id,
            request,
        });
        let plugin_install_telemetry = plugin_install_elicitation_telemetry_metadata(&event);
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        if let Some(plugin_install_telemetry) = plugin_install_telemetry {
            turn_context
                .session_telemetry
                .record_plugin_install_elicitation_sent(
                    plugin_install_telemetry.tool_type.as_str(),
                    plugin_install_telemetry.tool_id.as_str(),
                    plugin_install_telemetry.tool_name.as_str(),
                );
        }
        let _interactive_wait_guard = turn_context
            .turn_timing_state
            .begin_interactive_wait(InteractiveWaitKind::McpElicitation);
        McpServerElicitationOutcome {
            response: rx_response.await.ok(),
            sent: true,
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and manager fallback must stay serialized"
    )]
    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_elicitation(&server_name, &id)
                }
                None => None,
            }
        };
        if let Some(tx_response) = entry {
            tx_response
                .send(response)
                .map_err(|e| anyhow::anyhow!("failed to send elicitation response: {e:?}"))?;
            return Ok(());
        }

        self.services
            .latest_mcp_runtime()
            .manager_arc()
            .resolve_elicitation(server_name, id, response)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the projection lock must serialize the final compare-and-publish state transition"
    )]
    async fn refresh_mcp_servers_inner(
        &self,
        binding: &McpRequestBinding<'_>,
        mcp_projection: McpRuntimeProjection,
        environments: &TurnEnvironmentSnapshot,
        available_environment_ids: &[String],

        expected_runtime: Arc<McpRuntimeSnapshot>,
        bound_context: Option<McpRuntimeContext>,
    ) -> Arc<McpRuntimeSnapshot> {
        let auth = self.services.auth_manager.auth().await;
        let McpRuntimeProjection {
            config: mcp_config,
            plugins_available,
        } = mcp_projection;
        let mcp_config = Arc::new(mcp_config);
        let tool_plugin_provenance = codex_mcp::tool_plugin_provenance(&mcp_config);
        let mcp_servers = effective_mcp_servers(&mcp_config, auth.as_ref());
        let environment_manager = self.services.turn_environments.environment_manager();
        let cwd = environments
            .turn_environments
            .iter()
            .find(|environment| !environment.environment.is_remote())
            .and_then(|turn_environment| turn_environment.cwd().to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| expected_runtime.runtime_context().local_fallback_cwd());
        let mcp_runtime_context = bound_context.unwrap_or_else(|| {
            McpRuntimeContext::new(environment_manager, cwd)
                .with_selected_environments(environments.captured_environments())
        });
        let auth_statuses = compute_auth_statuses(
            mcp_servers.iter(),
            &mcp_config.codex_home,
            mcp_config.mcp_oauth_credentials_store_mode,
            mcp_config.auth_keyring_backend_kind,
            auth.as_ref(),
            &mcp_runtime_context,
        )
        .await;
        // Authentication discovery may have yielded to another publication.
        // Do not start clients for a candidate already known to have lost.
        // The final compare-and-publish below still covers later races.
        let current_runtime = self.services.latest_mcp_runtime();
        if !Arc::ptr_eq(&current_runtime, &expected_runtime) {
            return current_runtime;
        }
        let mut startup_cancellation = McpStartupCancellationGuard::new();
        let mcp_startup_cancellation_token = startup_cancellation.token();
        let published_startup_token = self
            .services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .clone();
        let cancellation_forwarding_stop = CancellationToken::new();
        let _cancellation_forwarder = tokio::spawn({
            let candidate_token = mcp_startup_cancellation_token.clone();
            let forwarding_stop = cancellation_forwarding_stop.clone();
            async move {
                tokio::select! {
                    biased;
                    _ = forwarding_stop.cancelled() => {}
                    _ = candidate_token.cancelled() => {}
                    _ = published_startup_token.cancelled() => candidate_token.cancel(),
                }
            }
        });
        let codex_apps_auth_manager =
            codex_mcp::host_owned_codex_apps_enabled(&mcp_config, auth.as_ref())
                .then(|| Arc::clone(&self.services.auth_manager));
        let refreshed_manager = McpConnectionManager::new(
            &mcp_servers,
            mcp_config.mcp_oauth_credentials_store_mode,
            mcp_config.auth_keyring_backend_kind,
            auth_statuses,
            binding.approval_policy,
            binding.sub_id.to_string(),
            self.get_tx_event(),
            mcp_startup_cancellation_token.clone(),
            (*binding.permission_profile).clone(),
            mcp_runtime_context.clone(),
            mcp_config.codex_home.clone(),
            self.services.mcp_manager.codex_apps_tools_cache(),
            codex_apps_tools_cache_key(
                auth.as_ref(),
                &mcp_config.chatgpt_base_url,
                mcp_config.apps_mcp_product_sku.as_deref(),
            ),
            mcp_config.prefix_mcp_tool_names,
            mcp_config.client_elicitation_capability.clone(),
            self.services
                .supports_openai_form_elicitation
                .load(std::sync::atomic::Ordering::Relaxed),
            tool_plugin_provenance,
            auth.as_ref(),
            codex_apps_auth_manager,
            Some(self.mcp_elicitation_lifecycle()),
            expected_runtime.manager().elicitation_router(),
            Some(expected_runtime.manager()),
        )
        .await;
        refreshed_manager
            .set_elicitations_auto_deny(expected_runtime.manager().elicitations_auto_deny());

        let Ok(projection_guard) = self.services.mcp_projection_lock.acquire().await else {
            unreachable!("MCP projection semaphore is never closed");
        };
        let current_runtime = self.services.latest_mcp_runtime();
        if !Arc::ptr_eq(&current_runtime, &expected_runtime) {
            drop(projection_guard);
            cancellation_forwarding_stop.cancel();
            refreshed_manager.shutdown().await;
            return current_runtime;
        }
        let mut published_token = self.services.mcp_startup_cancellation_token.lock().await;
        if published_token.is_cancelled() {
            drop(published_token);
            drop(projection_guard);
            cancellation_forwarding_stop.cancel();
            refreshed_manager.shutdown().await;
            return current_runtime;
        }
        *published_token = mcp_startup_cancellation_token;
        cancellation_forwarding_stop.cancel();
        drop(published_token);
        // The previous runtime may still be serving an in-flight step. Connections adopted by
        // this runtime remain live until the final manager lease is released.
        let mut state_owner = self.state.lock().await;
        let runtime = self.services.publish_mcp_runtime(
            &mut state_owner,
            mcp_config,
            plugins_available,
            mcp_runtime_context,
            available_environment_ids.to_vec(),
            refreshed_manager,
        );
        startup_cancellation.mark_published();
        runtime
    }

    pub(crate) async fn refresh_mcp_servers_if_requested(&self, turn_context: &TurnContext) {
        // Preparation is cancellable by the submission dispatcher. Keep the request
        // queued until its configuration is actually published, without holding the
        // pending-config lock across startup or adding a background retry owner.
        let requested = { self.pending_mcp_server_refresh_config.lock().await.clone() };
        let Some(requested) = requested else {
            return;
        };

        let McpServerRefreshConfig {
            mcp_servers,
            mcp_oauth_credentials_store_mode,
            auth_keyring_backend_kind,
        } = requested.as_ref().clone();

        let mcp_servers =
            match serde_json::from_value::<HashMap<String, McpServerConfig>>(mcp_servers) {
                Ok(servers) => servers,
                Err(err) => {
                    warn!("failed to parse MCP server refresh config: {err}");
                    self.acknowledge_mcp_server_refresh(&requested).await;
                    return;
                }
            };
        let store_mode = match serde_json::from_value::<OAuthCredentialsStoreMode>(
            mcp_oauth_credentials_store_mode,
        ) {
            Ok(mode) => mode,
            Err(err) => {
                warn!("failed to parse MCP OAuth refresh config: {err}");
                self.acknowledge_mcp_server_refresh(&requested).await;
                return;
            }
        };
        let keyring_backend_kind =
            match serde_json::from_value::<AuthKeyringBackendKind>(auth_keyring_backend_kind) {
                Ok(kind) => kind,
                Err(err) => {
                    warn!("failed to parse MCP auth keyring backend refresh config: {err}");
                    self.acknowledge_mcp_server_refresh(&requested).await;
                    return;
                }
            };

        self.services
            .latest_mcp_runtime()
            .manager()
            .invalidate_resource_caches();
        let mut refresh_config = self.get_config().await.as_ref().clone();
        refresh_config.mcp_oauth_credentials_store_mode = store_mode;
        let secret_auth_storage_enabled = match keyring_backend_kind {
            AuthKeyringBackendKind::Direct => false,
            AuthKeyringBackendKind::Secrets => true,
        };
        if let Err(err) = refresh_config
            .features
            .set_enabled(Feature::SecretAuthStorage, secret_auth_storage_enabled)
        {
            warn!("failed to apply MCP auth keyring backend refresh config: {err}");
            self.acknowledge_mcp_server_refresh(&requested).await;
            return;
        }

        let (mcp_projection, available_environment_ids, expected_runtime) = loop {
            let expected_runtime = self.services.latest_mcp_runtime();
            let available_environment_ids = expected_runtime.available_environment_ids().to_vec();
            let mut mcp_projection = self
                .services
                .mcp_manager
                .runtime_config_for_step(
                    &refresh_config,
                    &self.services.mcp_thread_init,
                    &self.services.thread_extension_data,
                    &turn_context.originator,
                    &available_environment_ids,
                )
                .await;
            mcp_projection.config.mcp_server_catalog = mcp_projection
                .config
                .mcp_server_catalog
                .with_materialized_servers(mcp_servers.clone());
            let expected_runtime = {
                let Ok(_guard) = self.services.mcp_projection_lock.acquire().await else {
                    unreachable!("MCP projection semaphore is never closed");
                };
                let current = self.services.latest_mcp_runtime();
                if !Arc::ptr_eq(&current, &expected_runtime) {
                    continue;
                }
                current
            };
            break (mcp_projection, available_environment_ids, expected_runtime);
        };
        let runtime = self
            .refresh_mcp_servers_inner(
                &McpRequestBinding::from_turn(turn_context),
                mcp_projection,
                &turn_context.environments,
                &available_environment_ids,
                expected_runtime,
                None,
            )
            .await;
        // A lost compare-and-publish race returns the winning runtime. Only
        // acknowledge if that authoritative runtime satisfies this request.
        if codex_mcp::configured_mcp_servers(runtime.config()) == mcp_servers
            && runtime.config().mcp_oauth_credentials_store_mode == store_mode
            && runtime.config().auth_keyring_backend_kind == keyring_backend_kind
        {
            self.acknowledge_mcp_server_refresh(&requested).await;
        }
    }

    async fn acknowledge_mcp_server_refresh(&self, applied: &Arc<McpServerRefreshConfig>) {
        let mut pending = self.pending_mcp_server_refresh_config.lock().await;
        // Value equality cannot distinguish a later identical refresh (for
        // example, a changed elicitation capability using the same server map).
        if pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, applied))
        {
            *pending = None;
        }
    }

    pub(crate) async fn set_openai_form_elicitation_support(
        &self,
        supported: bool,
    ) -> anyhow::Result<()> {
        if self
            .services
            .supports_openai_form_elicitation
            .load(std::sync::atomic::Ordering::Relaxed)
            == supported
        {
            return Ok(());
        }

        let config = self.get_config().await;
        let refresh_config = McpServerRefreshConfig {
            // The refresh replaces the whole catalog, so it needs the built-in, plugin and
            // extension servers too, not only the ones declared in config.
            mcp_servers: serde_json::to_value(self.runtime_mcp_servers(&config).await)?,
            mcp_oauth_credentials_store_mode: serde_json::to_value(
                config.mcp_oauth_credentials_store_mode,
            )?,
            auth_keyring_backend_kind: serde_json::to_value(config.auth_keyring_backend_kind())?,
        };
        self.services
            .supports_openai_form_elicitation
            .store(supported, std::sync::atomic::Ordering::Relaxed);
        *self.pending_mcp_server_refresh_config.lock().await = Some(Arc::new(refresh_config));
        Ok(())
    }

    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        turn_context: &TurnContext,
        refresh_config: &Config,
    ) {
        self.services
            .latest_mcp_runtime()
            .manager()
            .invalidate_resource_caches();
        let (mcp_projection, available_environment_ids, expected_runtime) = loop {
            let expected_runtime = self.services.latest_mcp_runtime();
            let available_environment_ids = expected_runtime.available_environment_ids().to_vec();
            let mcp_projection = self
                .services
                .mcp_manager
                .runtime_config_for_step(
                    refresh_config,
                    &self.services.mcp_thread_init,
                    &self.services.thread_extension_data,
                    &turn_context.originator,
                    &available_environment_ids,
                )
                .await;
            let expected_runtime = {
                let Ok(_guard) = self.services.mcp_projection_lock.acquire().await else {
                    unreachable!("MCP projection semaphore is never closed");
                };
                let current = self.services.latest_mcp_runtime();
                if !Arc::ptr_eq(&current, &expected_runtime) {
                    continue;
                }
                current
            };
            break (mcp_projection, available_environment_ids, expected_runtime);
        };
        self.refresh_mcp_servers_inner(
            &McpRequestBinding::from_turn(turn_context),
            mcp_projection,
            &turn_context.environments,
            &available_environment_ids,
            expected_runtime,
            None,
        )
        .await;
    }

    fn available_selected_environment_ids(
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> Vec<String> {
        let mut available = Vec::new();
        for root in selected_capability_roots {
            let CapabilityRootLocation::Environment { environment_id, .. } =
                &root.selected_root().location;
            if !available.contains(environment_id) {
                available.push(environment_id.clone());
            }
        }
        available
    }


    pub(crate) async fn cancel_mcp_startup(&self) {
        self.services
            .mcp_startup_cancellation_token
            .lock()
            .await
            .cancel();
    }
}

fn metadata_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

fn metadata_owned_string(meta: &Map<String, Value>, key: &str) -> Option<String> {
    metadata_str(meta, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn plugin_install_elicitation_telemetry_metadata(
    event: &EventMsg,
) -> Option<PluginInstallElicitationTelemetryMetadata> {
    let EventMsg::ElicitationRequest(ElicitationRequestEvent { request, .. }) = event else {
        return None;
    };
    let codex_protocol::approvals::ElicitationRequest::Form {
        meta: Some(Value::Object(meta)),
        ..
    } = request
    else {
        return None;
    };
    if metadata_str(meta, MCP_ELICITATION_APPROVAL_KIND_KEY)
        != Some(MCP_ELICITATION_APPROVAL_KIND_TOOL_SUGGESTION)
        || metadata_str(meta, TOOL_SUGGESTION_ACTION_KEY) != Some(TOOL_SUGGESTION_ACTION_INSTALL)
    {
        return None;
    }

    Some(PluginInstallElicitationTelemetryMetadata {
        tool_type: metadata_owned_string(meta, TOOL_SUGGESTION_TOOL_TYPE_KEY)?,
        tool_id: metadata_owned_string(meta, TOOL_SUGGESTION_TOOL_ID_KEY)?,
        tool_name: metadata_owned_string(meta, MCP_ELICITATION_TOOL_NAME_KEY)?,
    })
}

