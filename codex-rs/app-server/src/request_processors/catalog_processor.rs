use super::*;
use codex_core::config::permission_profile_catalog;
use codex_models_manager::manager::SharedModelsManager;
use futures::StreamExt;

#[derive(Clone)]
pub(crate) struct CatalogRequestProcessor {
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) skills_watcher: Arc<SkillsWatcher>,
    pub(super) auth_manager: Arc<AuthManager>,
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) models_manager: SharedModelsManager,
    pub(super) config: Arc<Config>,
    pub(super) config_manager: ConfigManager,
    pub(super) workspace_settings_cache: Arc<workspace_settings::WorkspaceSettingsCache>,
    current_catalogs: Arc<std::sync::Mutex<Vec<CurrentCatalog>>>,
    pagination: Arc<std::sync::Mutex<ModelPaginationState>>,
}

struct CurrentCatalog {
    provider_id: String,
    provider: codex_model_provider_info::ModelProviderInfo,
    http_clients: codex_http_client::HttpClientFactory,
    manager: SharedModelsManager,
    _worker: crate::models_refresh_worker::ModelsRefreshWorker,
}

struct ModelPaginationState {
    nonce: String,
    generation: u64,
    identity: Option<String>,
    snapshot: Option<(
        SharedModelsManager,
        Arc<Vec<codex_protocol::openai_models::ModelPreset>>,
    )>,
}

impl Default for ModelPaginationState {
    fn default() -> Self {
        Self {
            nonce: uuid::Uuid::now_v7().to_string(),
            generation: 0,
            identity: None,
            snapshot: None,
        }
    }
}

impl ModelPaginationState {
    fn bind(
        &mut self,
        manager: SharedModelsManager,
        models: Arc<Vec<codex_protocol::openai_models::ModelPreset>>,
        identity: Option<String>,
    ) {
        if self.identity == identity
            && self
                .snapshot
                .as_ref()
                .is_some_and(|(old_manager, old_models)| {
                    Arc::ptr_eq(old_manager, &manager)
                        && (Arc::ptr_eq(old_models, &models) || old_models == &models)
                })
        {
            return;
        }
        match self.generation.checked_add(1) {
            Some(generation) => self.generation = generation,
            None => {
                // Start a fresh cursor epoch rather than reuse a generation, so no
                // earlier cursor can resume against the newly bound catalog.
                self.nonce = uuid::Uuid::now_v7().to_string();
                self.generation = 0;
            }
        }
        self.identity = identity;
        self.snapshot = Some((manager, models));
    }

    fn cursor(&self, hidden: bool, index: usize) -> String {
        format!(
            "v1:{}:{}:{}:{index}",
            self.nonce,
            self.generation,
            u8::from(hidden)
        )
    }

    fn start(
        &self,
        cursor: Option<&str>,
        hidden: bool,
        total: usize,
    ) -> Result<usize, JSONRPCErrorError> {
        let Some(cursor) = cursor else {
            return Ok(0);
        };
        if cursor == "0" {
            return Ok(0);
        } // Legacy restart, never a continuation.
        let prefix = format!(
            "v1:{}:{}:{}:",
            self.nonce,
            self.generation,
            u8::from(hidden)
        );
        let Some(index) = cursor.strip_prefix(&prefix) else {
            if cursor.starts_with("v1:") || cursor.parse::<usize>().is_ok() {
                return Err(invalid_request(
                    "stale model cursor; restart model/list without a cursor",
                ));
            }
            return Err(invalid_request(format!("invalid cursor: {cursor}")));
        };
        let start = index
            .parse::<usize>()
            .map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))?;
        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total models {total}"
            )));
        }
        Ok(start)
    }
}

const SKILLS_LIST_CWD_CONCURRENCY: usize = 5;

fn skills_to_info(
    skills: &[codex_core::skills::SkillMetadata],
    disabled_paths: &HashSet<AbsolutePathBuf>,
) -> Vec<codex_app_server_protocol::SkillMetadata> {
    skills
        .iter()
        .map(|skill| {
            let enabled =
                skill.source.is_some() || !disabled_paths.contains(&skill.path_to_skills_md);
            codex_app_server_protocol::SkillMetadata {
                name: skill.name.clone(),
                description: skill.description.clone(),
                short_description: skill.short_description.clone(),
                interface: skill.interface.clone().map(Into::into),
                dependencies: skill.dependencies.clone().map(Into::into),
                path: skill.path_to_skills_md.clone(),
                scope: skill.scope.into(),
                enabled,
            }
        })
        .collect()
}

fn hooks_to_info(hooks: &[codex_hooks::HookListEntry]) -> Vec<HookMetadata> {
    hooks
        .iter()
        .map(|hook| HookMetadata {
            key: hook.key.clone(),
            event_name: hook.event_name.into(),
            handler_type: hook.handler_type.into(),
            matcher: hook.matcher.clone(),
            command: hook.command.clone(),
            timeout_sec: hook.timeout_sec,
            status_message: hook.status_message.clone(),
            source_path: hook.source_path.clone(),
            source: hook.source.into(),
            plugin_id: hook.plugin_id.clone(),
            display_order: hook.display_order,
            enabled: hook.enabled,
            is_managed: hook.is_managed,
            current_hash: hook.current_hash.clone(),
            trust_status: hook.trust_status.into(),
        })
        .collect()
}

fn errors_to_info(
    errors: &[codex_core::skills::SkillError],
) -> Vec<codex_app_server_protocol::SkillErrorInfo> {
    errors
        .iter()
        .map(|err| codex_app_server_protocol::SkillErrorInfo {
            path: err.path.to_path_buf(),
            message: err.message.clone(),
        })
        .collect()
}

impl CatalogRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        skills_watcher: Arc<SkillsWatcher>,
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        config: Arc<Config>,
        config_manager: ConfigManager,
        workspace_settings_cache: Arc<workspace_settings::WorkspaceSettingsCache>,
    ) -> Self {
        let models_manager = thread_manager.get_models_manager();
        Self {
            outgoing,
            skills_watcher,
            auth_manager,
            thread_manager,
            models_manager,
            config,
            config_manager,
            workspace_settings_cache,
            current_catalogs: Arc::default(),
            pagination: Arc::default(),
        }
    }

    pub(crate) async fn skills_list(
        &self,
        params: SkillsListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.skills_list_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn hooks_list(
        &self,
        params: HooksListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.hooks_list_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn skills_config_write(
        &self,
        params: SkillsConfigWriteParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.skills_config_write_response_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn skills_extra_roots_set(
        &self,
        params: SkillsExtraRootsSetParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.skills_extra_roots_set_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn model_list(
        &self,
        params: ModelListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let config = self.load_latest_config(None).await?;
        let http_clients = config.http_client_factory();
        let manager = if config.model_provider_id == self.config.model_provider_id
            && config.model_provider == self.config.model_provider
        {
            self.models_manager.clone()
        } else {
            let mut catalogs = self
                .current_catalogs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = catalogs.iter().find(|entry| {
                entry.provider_id == config.model_provider_id
                    && entry.provider == config.model_provider
                    && entry.http_clients == http_clients
            }) {
                entry.manager.clone()
            } else {
                let provider = codex_model_provider::create_model_provider(
                    config.model_provider.clone(),
                    Some(self.auth_manager.clone()),
                );
                // Manual catalogs belong to process startup, not subsequent config writes.
                let manager = provider.models_manager(
                    &config.model_provider_id,
                    config.codex_home.to_path_buf(),
                    self.config.model_catalog.clone(),
                );
                let worker = crate::models_refresh_worker::spawn(&manager, http_clients.clone());
                if catalogs.len() == 8 {
                    catalogs.remove(0);
                }
                catalogs.push(CurrentCatalog {
                    provider_id: config.model_provider_id,
                    provider: config.model_provider,
                    http_clients: http_clients.clone(),
                    manager: manager.clone(),
                    _worker: worker,
                });
                manager
            }
        };
        Self::list_models(manager, http_clients, params, &self.pagination)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn experimental_feature_list(
        &self,
        params: ExperimentalFeatureListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.experimental_feature_list_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn permission_profile_list(
        &self,
        params: PermissionProfileListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.permission_profile_list_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn collaboration_mode_list(
        &self,
        params: CollaborationModeListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Self::list_collaboration_modes(self.models_manager.clone(), params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn mock_experimental_method(
        &self,
        params: MockExperimentalMethodParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.mock_experimental_method_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    async fn resolve_cwd_config(
        &self,
        cwd: &Path,
    ) -> Result<(AbsolutePathBuf, ConfigLayerStack), String> {
        let cwd_abs =
            AbsolutePathBuf::relative_to_current_dir(cwd).map_err(|err| err.to_string())?;
        let config_layer_stack = self
            .config_manager
            .load_config_layers_for_cwd(cwd_abs.clone())
            .await
            .map_err(|err| err.to_string())?;

        Ok((cwd_abs, config_layer_stack))
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

    async fn list_models(
        models_manager: SharedModelsManager,
        http_client_factory: codex_http_client::HttpClientFactory,
        params: ModelListParams,
        pagination: &std::sync::Mutex<ModelPaginationState>,
    ) -> Result<ModelListResponse, JSONRPCErrorError> {
        let ModelListParams {
            limit,
            cursor,
            include_hidden,
        } = params;
        let identity = models_manager.catalog_identity();
        let snapshot = models_manager
            .list_models_shared(
                codex_models_manager::manager::RefreshStrategy::OnlineIfUncached,
                http_client_factory,
            )
            .await
            .map_err(|err| internal_error(format!("failed to list models: {err}")))?;
        if models_manager.catalog_identity() != identity {
            return Err(invalid_request(
                "model catalog identity changed; restart model/list without a cursor",
            ));
        }
        let hidden = include_hidden.unwrap_or(false);
        let models = snapshot
            .iter()
            .filter(|preset| hidden || preset.show_in_picker)
            .cloned()
            .map(crate::models::model_from_preset)
            .collect::<Vec<_>>();
        let total = models.len();
        let mut pagination = pagination
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pagination.bind(models_manager, snapshot, identity);
        let start = pagination.start(cursor.as_deref(), hidden, total)?;

        let effective_limit = limit.unwrap_or(total as u32).max(1) as usize;
        let effective_limit = effective_limit.min(total);

        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total models {total}"
            )));
        }

        let end = start.saturating_add(effective_limit).min(total);
        let items = models[start..end].to_vec();
        let next_cursor = if end < total {
            Some(pagination.cursor(hidden, end))
        } else {
            None
        };
        Ok(ModelListResponse {
            data: items,
            next_cursor,
        })
    }

    async fn list_collaboration_modes(
        models_manager: SharedModelsManager,
        params: CollaborationModeListParams,
    ) -> Result<CollaborationModeListResponse, JSONRPCErrorError> {
        let CollaborationModeListParams {} = params;
        let items = models_manager
            .list_collaboration_modes()
            .into_iter()
            .map(Into::into)
            .collect();
        let response = CollaborationModeListResponse { data: items };
        Ok(response)
    }

    async fn experimental_feature_list_response(
        &self,
        params: ExperimentalFeatureListParams,
    ) -> Result<ExperimentalFeatureListResponse, JSONRPCErrorError> {
        let ExperimentalFeatureListParams {
            cursor,
            limit,
            thread_id,
        } = params;
        let start = match cursor {
            Some(cursor) => match cursor.parse::<usize>() {
                Ok(idx) => idx,
                Err(_) => return Err(invalid_request(format!("invalid cursor: {cursor}"))),
            },
            None => 0,
        };
        let config = match thread_id.as_deref() {
            Some(thread_id) => {
                let thread_id = ThreadId::from_string(thread_id)
                    .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
                let thread = self
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;
                let thread_config = thread.config().await;
                self.config_manager
                    .load_latest_config_for_thread(thread_config.as_ref())
                    .await
                    .map_config_load_error()?
            }
            None => self.load_latest_config(/*fallback_cwd*/ None).await?,
        };
        let auth = self.auth_manager.auth().await;
        let workspace_codex_plugins_enabled = workspace_codex_plugins_enabled(
            &config,
            auth.as_ref(),
            Some(&self.workspace_settings_cache),
        )
        .await;

        let data = codex_features::user_settable_features()
            .map(|spec| {
                let (stage, display_name, description, announcement) = match spec.stage {
                    Stage::Experimental {
                        name,
                        menu_description,
                        announcement,
                    } => (
                        ApiExperimentalFeatureStage::Beta,
                        Some(name.to_string()),
                        Some(menu_description.to_string()),
                        Some(announcement.to_string()),
                    ),
                    Stage::UnderDevelopment => (
                        ApiExperimentalFeatureStage::UnderDevelopment,
                        None,
                        None,
                        None,
                    ),
                    Stage::Stable => (ApiExperimentalFeatureStage::Stable, None, None, None),
                    Stage::Deprecated => {
                        (ApiExperimentalFeatureStage::Deprecated, None, None, None)
                    }
                    Stage::Internal => (ApiExperimentalFeatureStage::Removed, None, None, None),
                };

                ApiExperimentalFeature {
                    name: spec.key.to_string(),
                    stage,
                    display_name,
                    description,
                    announcement,
                    enabled: config.features.enabled(spec.id)
                        && (workspace_codex_plugins_enabled
                            || !matches!(spec.id, Feature::Apps | Feature::Plugins)),
                    default_enabled: spec.default_enabled,
                    consumer: Some(match spec.consumer {
                        codex_features::FeatureConsumer::Runtime => {
                            codex_app_server_protocol::ExperimentalFeatureConsumer::Runtime
                        }
                        codex_features::FeatureConsumer::Client => {
                            codex_app_server_protocol::ExperimentalFeatureConsumer::Client
                        }
                    }),
                }
            })
            .collect::<Vec<_>>();

        let total = data.len();

        // Clamp to 1 so limit=0 cannot return a non-advancing page.
        let effective_limit = limit.unwrap_or(total as u32).max(1) as usize;
        let effective_limit = effective_limit.min(total);

        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total feature flags {total}"
            )));
        }

        let end = start.saturating_add(effective_limit).min(total);
        let data = data[start..end].to_vec();
        let next_cursor = if end < total {
            Some(end.to_string())
        } else {
            None
        };

        Ok(ExperimentalFeatureListResponse { data, next_cursor })
    }

    async fn permission_profile_list_response(
        &self,
        params: PermissionProfileListParams,
    ) -> Result<PermissionProfileListResponse, JSONRPCErrorError> {
        let PermissionProfileListParams { cursor, limit, cwd } = params;
        let config_layer_stack = match cwd {
            Some(cwd) => {
                let cwd = PathBuf::from(cwd);
                let (_, config_layer_stack) = self
                    .resolve_cwd_config(&cwd)
                    .await
                    .map_err(|err| internal_error(format!("failed to reload config: {err}")))?;
                config_layer_stack
            }
            None => self
                .config_manager
                .load_config_layers(/*cwd*/ None)
                .await
                .map_config_load_error()?,
        };
        let profiles = permission_profile_catalog(&config_layer_stack)
            .map_err(|err| internal_error(format!("failed to resolve permission profiles: {err}")))?
            .into_iter()
            .map(|profile| PermissionProfileSummary {
                id: profile.id,
                description: profile.description,
                allowed: profile.allowed,
            })
            .collect::<Vec<_>>();
        let total = profiles.len();
        let effective_limit = limit.unwrap_or(total as u32).max(1) as usize;
        let effective_limit = effective_limit.min(total);
        let start = match cursor {
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))?,
            None => 0,
        };

        if start > total {
            return Err(invalid_request(format!(
                "cursor {start} exceeds total permission profiles {total}"
            )));
        }

        let end = start.saturating_add(effective_limit).min(total);
        let data = profiles[start..end].to_vec();
        let next_cursor = (end < total).then_some(end.to_string());

        Ok(PermissionProfileListResponse { data, next_cursor })
    }

    async fn mock_experimental_method_inner(
        &self,
        params: MockExperimentalMethodParams,
    ) -> Result<MockExperimentalMethodResponse, JSONRPCErrorError> {
        let MockExperimentalMethodParams { value } = params;
        let response = MockExperimentalMethodResponse { echoed: value };
        Ok(response)
    }

    async fn skills_list_response(
        &self,
        params: SkillsListParams,
    ) -> Result<SkillsListResponse, JSONRPCErrorError> {
        let SkillsListParams { cwds, force_reload } = params;
        let cwds = if cwds.is_empty() {
            vec![self.config.cwd.to_path_buf()]
        } else {
            cwds
        };

        let config = self.load_latest_config(/*fallback_cwd*/ None).await?;
        let auth = self.auth_manager.auth().await;
        let workspace_codex_plugins_enabled = workspace_codex_plugins_enabled(
            &config,
            auth.as_ref(),
            Some(&self.workspace_settings_cache),
        )
        .await;
        let skills_service = self.thread_manager.skills_service();
        if force_reload {
            // One generation fence per RPC, before parallel cwd loads.
            skills_service.clear_cache();
        }
        let skill_snapshot_generation = skills_service.snapshot_generation();
        let plugin_discovery_generation = skills_service.reusable_snapshot_generation();
        let plugins_manager = self.thread_manager.plugins_manager();
        let fs = self
            .thread_manager
            .environment_manager()
            .default_environment()
            .map(|environment| environment.get_filesystem());
        let mut data = futures::stream::iter(cwds.into_iter().enumerate())
            .map(|(index, cwd)| {
                let config = &config;
                let fs = fs.clone();
                let plugins_manager = &plugins_manager;
                let skills_service = &skills_service;
                async move {
                    let (cwd_abs, config_layer_stack) = match self.resolve_cwd_config(&cwd).await {
                        Ok(resolved) => resolved,
                        Err(message) => {
                            let error_path = cwd.clone();
                            return (
                                index,
                                codex_app_server_protocol::SkillsListEntry {
                                    cwd,
                                    skills: Vec::new(),
                                    errors: vec![codex_app_server_protocol::SkillErrorInfo {
                                        path: error_path,
                                        message,
                                    }],
                                },
                            );
                        }
                    };
                    let (effective_skill_roots, plugin_skill_snapshots) = if workspace_codex_plugins_enabled {
                        let plugins_input = config.plugins_config_input();
                        plugins_manager
                            .effective_skill_roots_with_snapshots_for_layer_stack(
                                &config_layer_stack,
                                &plugins_input,
                                plugin_discovery_generation,
                            )
                            .await
                    } else {
                        (Vec::new(), None)
                    };
                    let skills_input = codex_core::skills::SkillsLoadInput::new(
                        cwd_abs.clone(),
                        effective_skill_roots,
                        config_layer_stack,
                        config.bundled_skills_enabled(),
                    );
                    let snapshot = skills_service
                        // The RPC already fenced both cache tiers once, before parallel loads.
                        .snapshot_for_cwd_with_plugin_skill_preload(
                            &skills_input,
                            false,
                            fs,
                            plugin_skill_snapshots.as_ref().map(|snapshots| {
                                (skill_snapshot_generation, snapshots)
                            }),
                        )
                        .await;
                    let outcome = snapshot.outcome();
                    let errors = errors_to_info(&outcome.errors);
                    let skills = skills_to_info(&outcome.skills, &outcome.disabled_paths);
                    (
                        index,
                        codex_app_server_protocol::SkillsListEntry {
                            cwd,
                            skills,
                            errors,
                        },
                    )
                }
            })
            .buffer_unordered(SKILLS_LIST_CWD_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        data.sort_unstable_by_key(|(index, _)| *index);
        let data = data.into_iter().map(|(_, entry)| entry).collect();
        Ok(SkillsListResponse { data })
    }

    async fn skills_extra_roots_set_response(
        &self,
        params: SkillsExtraRootsSetParams,
    ) -> Result<SkillsExtraRootsSetResponse, JSONRPCErrorError> {
        let SkillsExtraRootsSetParams { extra_roots } = params;
        self.skills_watcher
            .register_runtime_extra_roots(&extra_roots)
            .await
            .map_err(internal_error)?;
        self.thread_manager
            .skills_service()
            .set_extra_roots(extra_roots);
        self.outgoing
            .send_server_notification(ServerNotification::SkillsChanged(
                codex_app_server_protocol::SkillsChangedNotification {},
            ))
            .await;
        Ok(SkillsExtraRootsSetResponse {})
    }

    /// Handle `hooks/list` by resolving hooks for each requested cwd.
    async fn hooks_list_response(
        &self,
        params: HooksListParams,
    ) -> Result<HooksListResponse, JSONRPCErrorError> {
        let HooksListParams { cwds } = params;
        let cwds = if cwds.is_empty() {
            vec![self.config.cwd.to_path_buf()]
        } else {
            cwds
        };

        let auth = self.auth_manager.auth().await;
        let plugins_manager = self.thread_manager.plugins_manager();
        let jobs = cwds.into_iter().map(|cwd| {
            let auth = &auth;
            let plugins_manager = &plugins_manager;
            async move {
                let config = match self
                    .config_manager
                    .load_for_cwd(
                        /*request_overrides*/ None,
                        ConfigOverrides::default(),
                        Some(cwd.clone()),
                    )
                    .await
                {
                    Ok(config) => config,
                    Err(err) => {
                        let error_path = cwd.clone();
                        return codex_app_server_protocol::HooksListEntry {
                            cwd,
                            hooks: Vec::new(),
                            warnings: Vec::new(),
                            errors: vec![codex_app_server_protocol::HookErrorInfo {
                                path: error_path,
                                message: err.to_string(),
                            }],
                        };
                    }
                };
                let workspace_codex_plugins_enabled = workspace_codex_plugins_enabled(
                    &config,
                    auth.as_ref(),
                    Some(&self.workspace_settings_cache),
                )
                .await;
                let plugins_enabled =
                    config.features.enabled(Feature::Plugins) && workspace_codex_plugins_enabled;
                let plugin_hooks = if plugins_enabled {
                    let plugins_input = config.plugins_config_input();
                    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
                    codex_core_plugins::PluginHookLoadOutcome {
                        hook_sources: plugin_outcome.effective_plugin_hook_sources(),
                        hook_load_warnings: plugin_outcome.effective_plugin_hook_warnings(),
                    }
                } else {
                    codex_core_plugins::PluginHookLoadOutcome::default()
                };
                let hooks_config = codex_hooks::HooksConfig {
                    feature_enabled: config.features.enabled(Feature::CodexHooks),
                    bypass_hook_trust: config.bypass_hook_trust,
                    config_layer_stack: Some(config.config_layer_stack.as_ref().clone()),
                    plugin_hook_sources: plugin_hooks.hook_sources,
                    plugin_hook_load_warnings: plugin_hooks.hook_load_warnings,
                    ..Default::default()
                };
                let hooks = match tokio::task::spawn_blocking(move || {
                    codex_hooks::list_hooks(hooks_config)
                })
                .await
                {
                    Ok(hooks) => hooks,
                    Err(err) => {
                        return codex_app_server_protocol::HooksListEntry {
                            errors: vec![codex_app_server_protocol::HookErrorInfo {
                                path: cwd.clone(),
                                message: format!("hook discovery task failed: {err}"),
                            }],
                            cwd,
                            hooks: Vec::new(),
                            warnings: Vec::new(),
                        };
                    }
                };
                codex_app_server_protocol::HooksListEntry {
                    cwd,
                    hooks: hooks_to_info(&hooks.hooks),
                    warnings: hooks.warnings,
                    errors: Vec::new(),
                }
            }
        });
        let data = collect_hook_entries(jobs).await;
        Ok(HooksListResponse { data })
    }

    async fn skills_config_write_response_inner(
        &self,
        params: SkillsConfigWriteParams,
    ) -> Result<SkillsConfigWriteResponse, JSONRPCErrorError> {
        let SkillsConfigWriteParams {
            path,
            name,
            enabled,
        } = params;
        let edit = match (path, name) {
            (Some(path), None) => ConfigEdit::SetSkillConfig {
                path: path.into_path_buf(),
                enabled,
            },
            (None, Some(name)) if !name.trim().is_empty() => {
                ConfigEdit::SetSkillConfigByName { name, enabled }
            }
            _ => {
                return Err(invalid_params(
                    "skills/config/write requires exactly one of path or name",
                ));
            }
        };
        let edits = vec![edit];
        ConfigEditsBuilder::new(&self.config.codex_home)
            .with_edits(edits)
            .apply()
            .await
            .map(|()| {
                // This edit bypasses ConfigManager, so publish it to the next read.
                self.config_manager.invalidate_load_cache();
                self.thread_manager.plugins_manager().clear_cache();
                self.thread_manager.skills_service().clear_cache();
                SkillsConfigWriteResponse {
                    effective_enabled: enabled,
                }
            })
            .map_err(|err| internal_error(format!("failed to update skill settings: {err}")))
    }
}

// Free completed slots immediately; response ordering is restored only after
// every directory has been inspected, including entries containing errors.
async fn collect_hook_entries<'a, T, I, F>(jobs: I) -> Vec<T>
where
    T: Send + 'a,
    I: IntoIterator<Item = F>,
    I::IntoIter: Send,
    F: std::future::Future<Output = T> + Send + 'a,
{
    let mut jobs = jobs.into_iter().enumerate();
    let mut pending =
        futures::stream::FuturesUnordered::<futures::future::BoxFuture<'a, (usize, T)>>::new();
    let mut entries = Vec::new();
    loop {
        // Erase each admitted future without retaining a higher-ranked map
        // adapter across await. At most the concurrency limit is allocated.
        while pending.len() < SKILLS_LIST_CWD_CONCURRENCY {
            let Some((index, job)) = jobs.next() else {
                break;
            };
            pending.push(Box::pin(async move { (index, job.await) }));
        }
        let Some(entry) = pending.next().await else {
            break;
        };
        entries.push(entry);
    }
    entries.sort_unstable_by_key(|(index, _)| *index);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

