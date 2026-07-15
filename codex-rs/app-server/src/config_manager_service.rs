use crate::config_layer::config_layer_metadata_to_api;
use crate::config_layer::config_layer_to_api;
use crate::config_manager::ConfigManager;
use codex_app_server_protocol::AnalyticsConfig as ApiAnalyticsConfig;
use codex_app_server_protocol::AppConfig as ApiAppConfig;
use codex_app_server_protocol::AppToolApproval as ApiAppToolApproval;
use codex_app_server_protocol::AppToolConfig as ApiAppToolConfig;
use codex_app_server_protocol::AppToolsConfig as ApiAppToolsConfig;
use codex_app_server_protocol::AppsConfig as ApiAppsConfig;
use codex_app_server_protocol::AppsDefaultConfig as ApiAppsDefaultConfig;
use codex_app_server_protocol::Config as ApiConfig;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigReadParams;
use codex_app_server_protocol::ConfigReadResponse;
use codex_app_server_protocol::ConfigValueWriteParams;
use codex_app_server_protocol::ConfigWriteErrorCode;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::ForcedChatgptWorkspaceIds as ApiForcedWorkspaceIds;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::OverriddenMetadata;
use codex_app_server_protocol::SandboxWorkspaceWrite as ApiSandboxWorkspaceWrite;
use codex_app_server_protocol::ToolsV2 as ApiToolsV2;
use codex_app_server_protocol::WriteStatus;
use codex_config::AppToolApproval as CoreAppToolApproval;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerMetadata;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::ConfigLayerStackOrdering;
use codex_config::ConfigRequirementsToml;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::ForcedChatgptWorkspaceIds as CoreForcedWorkspaceIds;
use codex_config::config_toml::ToolsToml;
use codex_config::merge_toml_values;
use codex_config::types::AnalyticsConfigToml;
use codex_config::types::AppConfig as CoreAppConfig;
use codex_config::types::AppToolConfig as CoreAppToolConfig;
use codex_config::types::AppToolsConfig as CoreAppToolsConfig;
use codex_config::types::AppsConfigToml;
use codex_config::types::AppsDefaultConfig as CoreAppsDefaultConfig;
use codex_config::types::SandboxWorkspaceWrite as CoreSandboxWorkspaceWrite;
use codex_core::config::deserialize_config_toml_with_base;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::validate_feature_requirements_for_config_toml;
use codex_core::path_utils;
use codex_core::path_utils::SymlinkWritePaths;
use codex_core::path_utils::resolve_symlink_write_paths;
use codex_core::path_utils::write_atomically;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value as JsonValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use tokio::task;
use toml::Value as TomlValue;
use toml_edit::Item as TomlItem;

#[derive(Debug, Error)]
pub(crate) enum ConfigManagerError {
    #[error("{message}")]
    Write {
        code: ConfigWriteErrorCode,
        message: String,
    },

    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{context}: {source}")]
    Json {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("{context}: {source}")]
    Toml {
        context: &'static str,
        #[source]
        source: toml::de::Error,
    },

    #[error("{context}: {source}")]
    Anyhow {
        context: &'static str,
        #[source]
        source: anyhow::Error,
    },
}

impl ConfigManagerError {
    fn write(code: ConfigWriteErrorCode, message: impl Into<String>) -> Self {
        Self::Write {
            code,
            message: message.into(),
        }
    }

    fn io(context: &'static str, source: std::io::Error) -> Self {
        Self::Io { context, source }
    }

    fn json(context: &'static str, source: serde_json::Error) -> Self {
        Self::Json { context, source }
    }

    fn toml(context: &'static str, source: toml::de::Error) -> Self {
        Self::Toml { context, source }
    }

    fn anyhow(context: &'static str, source: anyhow::Error) -> Self {
        Self::Anyhow { context, source }
    }

    pub(crate) fn write_error_code(&self) -> Option<ConfigWriteErrorCode> {
        match self {
            Self::Write { code, .. } => Some(code.clone()),
            _ => None,
        }
    }
}

fn api_sandbox_workspace_write(value: CoreSandboxWorkspaceWrite) -> ApiSandboxWorkspaceWrite {
    let CoreSandboxWorkspaceWrite {
        writable_roots,
        network_access,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
    } = value;

    ApiSandboxWorkspaceWrite {
        writable_roots: writable_roots
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect(),
        network_access,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
    }
}

fn api_forced_workspace_ids(value: CoreForcedWorkspaceIds) -> ApiForcedWorkspaceIds {
    match value {
        CoreForcedWorkspaceIds::Single(value) => ApiForcedWorkspaceIds::Single(value),
        CoreForcedWorkspaceIds::Multiple(values) => ApiForcedWorkspaceIds::Multiple(values),
    }
}

fn api_tools(value: ToolsToml) -> ApiToolsV2 {
    let ToolsToml {
        web_search,
        experimental_request_user_input: _,
    } = value;
    ApiToolsV2 { web_search }
}

fn api_analytics(value: AnalyticsConfigToml) -> ApiAnalyticsConfig {
    let AnalyticsConfigToml { enabled } = value;
    ApiAnalyticsConfig {
        enabled,
        additional: HashMap::new(),
    }
}

fn api_app_tool_approval(value: CoreAppToolApproval) -> ApiAppToolApproval {
    match value {
        CoreAppToolApproval::Auto => ApiAppToolApproval::Auto,
        CoreAppToolApproval::Prompt => ApiAppToolApproval::Prompt,
        CoreAppToolApproval::Writes => ApiAppToolApproval::Writes,
        CoreAppToolApproval::Approve => ApiAppToolApproval::Approve,
    }
}

fn api_app_tool(value: CoreAppToolConfig) -> ApiAppToolConfig {
    let CoreAppToolConfig {
        enabled,
        approval_mode,
    } = value;
    ApiAppToolConfig {
        enabled,
        approval_mode: approval_mode.map(api_app_tool_approval),
    }
}

fn api_app_tools(value: CoreAppToolsConfig) -> ApiAppToolsConfig {
    let CoreAppToolsConfig { tools } = value;
    ApiAppToolsConfig {
        tools: tools
            .into_iter()
            .map(|(name, config)| (name, api_app_tool(config)))
            .collect(),
    }
}

fn api_app(value: CoreAppConfig) -> ApiAppConfig {
    let CoreAppConfig {
        enabled,
        approvals_reviewer,
        destructive_enabled,
        open_world_enabled,
        default_tools_approval_mode,
        default_tools_enabled,
        tools,
    } = value;

    ApiAppConfig {
        enabled,
        approvals_reviewer: approvals_reviewer.map(Into::into),
        destructive_enabled,
        open_world_enabled,
        default_tools_approval_mode: default_tools_approval_mode.map(api_app_tool_approval),
        default_tools_enabled,
        tools: tools.map(api_app_tools),
    }
}

fn api_apps_default(value: CoreAppsDefaultConfig) -> ApiAppsDefaultConfig {
    let CoreAppsDefaultConfig {
        enabled,
        approvals_reviewer,
        destructive_enabled,
        open_world_enabled,
        default_tools_approval_mode,
    } = value;

    ApiAppsDefaultConfig {
        enabled,
        approvals_reviewer: approvals_reviewer.map(Into::into),
        destructive_enabled,
        open_world_enabled,
        default_tools_approval_mode: default_tools_approval_mode.map(api_app_tool_approval),
    }
}

fn api_apps(value: AppsConfigToml) -> ApiAppsConfig {
    let AppsConfigToml { default, apps } = value;
    ApiAppsConfig {
        default: default.map(api_apps_default),
        apps: apps
            .into_iter()
            .map(|(id, config)| (id, api_app(config)))
            .collect(),
    }
}

pub(crate) fn api_config_from_config_toml(
    config: ConfigToml,
) -> Result<ApiConfig, serde_json::Error> {
    let ConfigToml {
        model,
        review_model,
        model_provider,
        model_context_window,
        model_auto_compact_token_limit,
        model_auto_compact_token_limit_scope,
        approval_policy,
        approvals_reviewer,
        auto_review,
        shell_environment_policy,
        allow_login_shell,
        sandbox_mode,
        sandbox_workspace_write,
        default_permissions,
        permissions,
        notify,
        instructions,
        developer_instructions,
        include_permissions_instructions,
        include_apps_instructions,
        include_collaboration_mode_instructions,
        include_environment_context,
        model_instructions_file,
        compact_prompt,
        forced_chatgpt_workspace_id,
        forced_login_method,
        cli_auth_credentials_store,
        mcp_servers,
        mcp_oauth_credentials_store,
        mcp_oauth_callback_port,
        mcp_oauth_callback_url,
        model_providers,
        project_doc_max_bytes,
        project_doc_fallback_filenames,
        tool_output_token_limit,
        background_terminal_max_timeout,
        js_repl_node_path,
        js_repl_node_module_dirs,
        profile,
        profiles,
        history,
        sqlite_home,
        log_dir,
        debug,
        file_opener,
        tui,
        hide_agent_reasoning,
        show_raw_agent_reasoning,
        model_reasoning_effort,
        plan_mode_reasoning_effort,
        model_reasoning_summary,
        model_verbosity,
        model_supports_reasoning_summaries,
        model_catalog_json,
        personality,
        service_tier,
        chatgpt_base_url,
        apps_mcp_product_sku,
        orchestrator,
        openai_base_url,
        audio,
        experimental_realtime_ws_base_url,
        experimental_realtime_webrtc_call_base_url,
        experimental_realtime_ws_model,
        realtime,
        experimental_realtime_ws_backend_prompt,
        experimental_realtime_ws_startup_context,
        experimental_realtime_start_instructions,
        experimental_thread_config_endpoint,
        experimental_thread_store_endpoint,
        experimental_thread_store,
        projects,
        web_search,
        tools,
        tool_suggest,
        agents,
        memories,
        skills,
        hooks,
        plugins,
        marketplaces,
        features,
        suppress_unstable_features_warning,
        ghost_snapshot,
        project_root_markers,
        check_for_update_on_startup,
        disable_paste_burst,
        analytics,
        feedback,
        apps,
        desktop,
        otel,
        windows,
        notice,
        experimental_compact_prompt_file,
        experimental_use_unified_exec_tool,
        oss_provider,
    } = config;

    let mut additional = HashMap::new();
    macro_rules! additional {
        ($($field:ident),* $(,)?) => {
            $(
                additional.insert(
                    stringify!($field).to_owned(),
                    serde_json::to_value($field)?,
                );
            )*
        };
    }

    additional!(
        auto_review,
        shell_environment_policy,
        allow_login_shell,
        default_permissions,
        permissions,
        notify,
        include_permissions_instructions,
        include_apps_instructions,
        include_collaboration_mode_instructions,
        include_environment_context,
        model_instructions_file,
        cli_auth_credentials_store,
        mcp_servers,
        mcp_oauth_credentials_store,
        mcp_oauth_callback_port,
        mcp_oauth_callback_url,
        model_providers,
        project_doc_max_bytes,
        project_doc_fallback_filenames,
        tool_output_token_limit,
        background_terminal_max_timeout,
        js_repl_node_path,
        js_repl_node_module_dirs,
        profile,
        profiles,
        history,
        sqlite_home,
        log_dir,
        debug,
        file_opener,
        tui,
        hide_agent_reasoning,
        show_raw_agent_reasoning,
        plan_mode_reasoning_effort,
        model_supports_reasoning_summaries,
        model_catalog_json,
        personality,
        chatgpt_base_url,
        apps_mcp_product_sku,
        orchestrator,
        openai_base_url,
        audio,
        experimental_realtime_ws_base_url,
        experimental_realtime_webrtc_call_base_url,
        experimental_realtime_ws_model,
        realtime,
        experimental_realtime_ws_backend_prompt,
        experimental_realtime_ws_startup_context,
        experimental_realtime_start_instructions,
        experimental_thread_config_endpoint,
        experimental_thread_store_endpoint,
        experimental_thread_store,
        projects,
        tool_suggest,
        agents,
        memories,
        skills,
        hooks,
        plugins,
        marketplaces,
        features,
        suppress_unstable_features_warning,
        ghost_snapshot,
        project_root_markers,
        check_for_update_on_startup,
        disable_paste_burst,
        feedback,
        otel,
        windows,
        notice,
        experimental_compact_prompt_file,
        experimental_use_unified_exec_tool,
        oss_provider,
    );

    Ok(ApiConfig {
        model,
        review_model,
        model_context_window,
        model_auto_compact_token_limit,
        model_auto_compact_token_limit_scope,
        model_provider,
        approval_policy: approval_policy.map(Into::into),
        approvals_reviewer: approvals_reviewer.map(Into::into),
        sandbox_mode: sandbox_mode.map(Into::into),
        sandbox_workspace_write: sandbox_workspace_write.map(api_sandbox_workspace_write),
        forced_chatgpt_workspace_id: forced_chatgpt_workspace_id.map(api_forced_workspace_ids),
        forced_login_method,
        web_search,
        tools: tools.map(api_tools),
        instructions,
        developer_instructions,
        compact_prompt,
        model_reasoning_effort,
        model_reasoning_summary,
        model_verbosity,
        service_tier,
        analytics: analytics.map(api_analytics),
        apps: apps.map(api_apps),
        desktop,
        additional,
    })
}

impl ConfigManager {
    pub(crate) async fn read(
        &self,
        params: ConfigReadParams,
    ) -> Result<ConfigReadResponse, ConfigManagerError> {
        let layers = match params.cwd.as_deref() {
            Some(cwd) => {
                let cwd = AbsolutePathBuf::try_from(PathBuf::from(cwd)).map_err(|err| {
                    ConfigManagerError::io("failed to resolve config cwd to an absolute path", err)
                })?;
                self.load_config_layers(Some(cwd)).await.map_err(|err| {
                    ConfigManagerError::io("failed to read configuration layers", err)
                })?
            }
            None => self.load_thread_agnostic_config().await.map_err(|err| {
                ConfigManagerError::io("failed to read configuration layers", err)
            })?,
        };

        let effective = layers.effective_config();
        let effective_config_toml: ConfigToml = effective
            .try_into()
            .map_err(|err| ConfigManagerError::toml("invalid configuration", err))?;

        let config = api_config_from_config_toml(effective_config_toml)
            .map_err(|err| ConfigManagerError::json("failed to serialize configuration", err))?;

        Ok(ConfigReadResponse {
            config,
            origins: layers
                .origins()
                .into_iter()
                .map(|(path, metadata)| (path, config_layer_metadata_to_api(metadata)))
                .collect(),
            layers: params.include_layers.then(|| {
                layers
                    .get_layers(
                        ConfigLayerStackOrdering::HighestPrecedenceFirst,
                        /*include_disabled*/ true,
                    )
                    .iter()
                    .map(|layer| config_layer_to_api(layer.as_layer()))
                    .collect()
            }),
        })
    }

    pub(crate) async fn read_requirements(
        &self,
    ) -> Result<Option<ConfigRequirementsToml>, ConfigManagerError> {
        let layers = self
            .load_thread_agnostic_config()
            .await
            .map_err(|err| ConfigManagerError::io("failed to read configuration layers", err))?;

        let requirements = layers.requirements_toml().clone();
        if requirements.is_empty() {
            Ok(None)
        } else {
            Ok(Some(requirements))
        }
    }

    pub(crate) async fn write_value(
        &self,
        params: ConfigValueWriteParams,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let edits = vec![(params.key_path, params.value, params.merge_strategy)];
        self.apply_edits(params.file_path, params.expected_version, edits)
            .await
    }

    pub(crate) async fn batch_write(
        &self,
        params: ConfigBatchWriteParams,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let edits = params
            .edits
            .into_iter()
            .map(|edit| (edit.key_path, edit.value, edit.merge_strategy))
            .collect();

        self.apply_edits(params.file_path, params.expected_version, edits)
            .await
    }

    async fn apply_edits(
        &self,
        file_path: Option<String>,
        expected_version: Option<String>,
        edits: Vec<(String, JsonValue, MergeStrategy)>,
    ) -> Result<ConfigWriteResponse, ConfigManagerError> {
        let allowed_path = self
            .user_config_path()
            .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?;
        let provided_path = match file_path {
            Some(path) => AbsolutePathBuf::from_absolute_path(PathBuf::from(path))
                .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?,
            None => allowed_path.clone(),
        };

        if !paths_match(&allowed_path, &provided_path) {
            return Err(ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigLayerReadonly,
                "Only writes to the user config are allowed",
            ));
        }

        let layers = self
            .load_thread_agnostic_config()
            .await
            .map_err(|err| ConfigManagerError::io("failed to load configuration", err))?;
        let user_layer = match layers.get_active_user_layer() {
            Some(layer) => Cow::Borrowed(layer),
            None => Cow::Owned(create_empty_user_layer(&allowed_path).await?),
        };

        if let Some(expected) = expected_version.as_deref()
            && expected != user_layer.version
        {
            return Err(ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigVersionConflict,
                "Configuration was modified since last read. Fetch latest version and retry.",
            ));
        }

        let mut user_config = user_layer.config.clone();
        let mut parsed_segments = Vec::new();
        let mut config_edits = Vec::new();

        for (key_path, value, strategy) in edits.into_iter() {
            let segments = parse_key_path(&key_path).map_err(|message| {
                ConfigManagerError::write(ConfigWriteErrorCode::ConfigValidationError, message)
            })?;
            if !value.is_null() {
                match segments.as_slice() {
                    [segment] if segment == "profile" => {
                        return Err(ConfigManagerError::write(
                            ConfigWriteErrorCode::ConfigValidationError,
                            "`profile` is a legacy config selector and can no longer be written; use `--profile <name>` with `<name>.config.toml` instead",
                        ));
                    }
                    [segment, ..] if segment == "profiles" => {
                        return Err(ConfigManagerError::write(
                            ConfigWriteErrorCode::ConfigValidationError,
                            "`profiles` contains legacy config profile tables and can no longer be written; use `--profile <name>` with `<name>.config.toml` instead",
                        ));
                    }
                    _ => {}
                }
            }
            let original_value = value_at_path(&user_config, &segments).cloned();
            let parsed_value = parse_value(value).map_err(|message| {
                ConfigManagerError::write(ConfigWriteErrorCode::ConfigValidationError, message)
            })?;

            apply_merge(&mut user_config, &segments, parsed_value.as_ref(), strategy).map_err(
                |err| match err {
                    MergeError::Validation(message) => ConfigManagerError::write(
                        ConfigWriteErrorCode::ConfigValidationError,
                        message,
                    ),
                },
            )?;

            let updated_value = value_at_path(&user_config, &segments).cloned();
            if original_value != updated_value {
                let edit = match updated_value {
                    Some(value) => ConfigEdit::SetPath {
                        segments: segments.clone(),
                        value: toml_value_to_item(&value).map_err(|err| {
                            ConfigManagerError::anyhow("failed to build config edits", err)
                        })?,
                    },
                    None => ConfigEdit::ClearPath {
                        segments: segments.clone(),
                    },
                };
                config_edits.push(edit);
            }

            parsed_segments.push(segments);
        }

        let user_config_toml =
            deserialize_config_toml_with_base(user_config.clone(), self.codex_home()).map_err(
                |err| {
                    ConfigManagerError::write(
                        ConfigWriteErrorCode::ConfigValidationError,
                        format!("Invalid configuration: {err}"),
                    )
                },
            )?;
        validate_feature_requirements_for_config_toml(
            &user_config_toml,
            layers.requirements().feature_requirements.as_ref(),
        )
        .map_err(|err| {
            ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigValidationError,
                format!("Invalid configuration: {err}"),
            )
        })?;
        let updated_layers = layers.with_user_config(&provided_path, user_config.clone());
        let effective = updated_layers.effective_config();
        deserialize_config_toml_with_base(effective.clone(), self.codex_home()).map_err(|err| {
            ConfigManagerError::write(
                ConfigWriteErrorCode::ConfigValidationError,
                format!("Invalid configuration: {err}"),
            )
        })?;

        if !config_edits.is_empty() {
            ConfigEditsBuilder::for_config_path(provided_path.as_path())
                .with_edits(config_edits)
                .apply()
                .await
                .map_err(|err| ConfigManagerError::anyhow("failed to persist config.toml", err))?;
        }

        let overridden = first_overridden_edit(&updated_layers, &effective, &parsed_segments);
        let status = overridden
            .as_ref()
            .map(|_| WriteStatus::OkOverridden)
            .unwrap_or(WriteStatus::Ok);

        Ok(ConfigWriteResponse {
            status,
            version: updated_layers
                .get_active_user_layer()
                .ok_or_else(|| {
                    ConfigManagerError::write(
                        ConfigWriteErrorCode::UserLayerNotFound,
                        "user layer not found in updated layers",
                    )
                })?
                .version
                .clone(),
            file_path: provided_path,
            overridden_metadata: overridden,
        })
    }

    /// Loads a "thread-agnostic" config, which means the config layers do not
    /// include any in-repo .codex/ folders because there is no cwd/project root
    /// associated with this query.
    async fn load_thread_agnostic_config(&self) -> std::io::Result<ConfigLayerStack> {
        self.load_config_layers(/*cwd*/ None).await
    }
}

async fn create_empty_user_layer(
    config_toml: &AbsolutePathBuf,
) -> Result<ConfigLayerEntry, ConfigManagerError> {
    let SymlinkWritePaths {
        read_path,
        write_path,
    } = resolve_symlink_write_paths(config_toml.as_path())
        .map_err(|err| ConfigManagerError::io("failed to resolve user config path", err))?;
    let toml_value = match read_path {
        Some(path) => match tokio::fs::read_to_string(&path).await {
            Ok(contents) => toml::from_str(&contents).map_err(|e| {
                ConfigManagerError::toml("failed to parse existing user config.toml", e)
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                write_empty_user_config(write_path.clone()).await?;
                TomlValue::Table(toml::map::Map::new())
            }
            Err(err) => {
                return Err(ConfigManagerError::io(
                    "failed to read user config.toml",
                    err,
                ));
            }
        },
        None => {
            write_empty_user_config(write_path).await?;
            TomlValue::Table(toml::map::Map::new())
        }
    };
    Ok(ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: config_toml.clone(),
            profile: None,
        },
        toml_value,
    ))
}

async fn write_empty_user_config(write_path: PathBuf) -> Result<(), ConfigManagerError> {
    task::spawn_blocking(move || write_atomically(&write_path, ""))
        .await
        .map_err(|err| ConfigManagerError::anyhow("config persistence task panicked", err.into()))?
        .map_err(|err| ConfigManagerError::io("failed to create empty user config.toml", err))
}

fn parse_value(value: JsonValue) -> Result<Option<TomlValue>, String> {
    if value.is_null() {
        return Ok(None);
    }

    serde_json::from_value::<TomlValue>(value)
        .map(Some)
        .map_err(|err| format!("invalid value: {err}"))
}

fn parse_key_path(path: &str) -> Result<Vec<String>, String> {
    if path.trim().is_empty() {
        return Err("keyPath must not be empty".to_string());
    }

    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut chars = path.chars();
    let mut quoted = false;

    // Split on dots unless they appear inside a quoted segment. Bare segments
    // intentionally stay permissive so existing paths like `sample@catalog`
    // remain valid.
    while let Some(ch) = chars.next() {
        match ch {
            '"' if segment.is_empty() && !quoted => quoted = true,
            '"' if quoted => quoted = false,
            '\\' if quoted => {
                // Quoted segments may escape punctuation that would otherwise
                // participate in parsing, such as `.` or `"`.
                let Some(escaped) = chars.next() else {
                    return Err("unterminated escape in keyPath".to_string());
                };
                segment.push(escaped);
            }
            '.' if !quoted => {
                if segment.is_empty() {
                    return Err("keyPath segments must not be empty".to_string());
                }
                segments.push(std::mem::take(&mut segment));
            }
            '"' => return Err("invalid quoted keyPath segment".to_string()),
            _ => segment.push(ch),
        }
    }

    if quoted {
        return Err("unterminated quoted keyPath segment".to_string());
    }
    if segment.is_empty() {
        return Err("keyPath segments must not be empty".to_string());
    }

    segments.push(segment);
    Ok(segments)
}

#[derive(Debug)]
enum MergeError {
    Validation(String),
}

fn apply_merge(
    root: &mut TomlValue,
    segments: &[String],
    value: Option<&TomlValue>,
    strategy: MergeStrategy,
) -> Result<bool, MergeError> {
    let Some(value) = value else {
        return clear_path(root, segments);
    };

    let Some((last, parents)) = segments.split_last() else {
        return Err(MergeError::Validation(
            "keyPath must not be empty".to_string(),
        ));
    };

    let mut current = root;

    for segment in parents {
        match current {
            TomlValue::Table(table) => {
                current = table
                    .entry(segment.clone())
                    .or_insert_with(|| TomlValue::Table(toml::map::Map::new()));
            }
            _ => {
                *current = TomlValue::Table(toml::map::Map::new());
                if let TomlValue::Table(table) = current {
                    current = table
                        .entry(segment.clone())
                        .or_insert_with(|| TomlValue::Table(toml::map::Map::new()));
                }
            }
        }
    }

    let table = current.as_table_mut().ok_or_else(|| {
        MergeError::Validation("cannot set value on non-table parent".to_string())
    })?;

    if matches!(strategy, MergeStrategy::Upsert)
        && let Some(existing) = table.get_mut(last)
        && matches!(existing, TomlValue::Table(_))
        && matches!(value, TomlValue::Table(_))
    {
        merge_toml_values(existing, value);
        return Ok(true);
    }

    let changed = table
        .get(last)
        .map(|existing| Some(existing) != Some(value))
        .unwrap_or(true);
    table.insert(last.clone(), value.clone());
    Ok(changed)
}

fn clear_path(root: &mut TomlValue, segments: &[String]) -> Result<bool, MergeError> {
    let Some((last, parents)) = segments.split_last() else {
        return Err(MergeError::Validation(
            "keyPath must not be empty".to_string(),
        ));
    };

    let mut current = root;
    for segment in parents {
        match current {
            TomlValue::Table(table) => {
                let Some(next) = table.get_mut(segment) else {
                    return Ok(false);
                };
                current = next;
            }
            _ => return Ok(false),
        }
    }

    let Some(parent) = current.as_table_mut() else {
        return Ok(false);
    };

    Ok(parent.remove(last).is_some())
}

fn toml_value_to_item(value: &TomlValue) -> anyhow::Result<TomlItem> {
    match value {
        TomlValue::Table(table) => {
            let mut table_item = toml_edit::Table::new();
            table_item.set_implicit(false);
            for (key, val) in table {
                table_item.insert(key, toml_value_to_item(val)?);
            }
            Ok(TomlItem::Table(table_item))
        }
        other => Ok(TomlItem::Value(toml_value_to_value(other)?)),
    }
}

fn toml_value_to_value(value: &TomlValue) -> anyhow::Result<toml_edit::Value> {
    match value {
        TomlValue::String(val) => Ok(toml_edit::Value::from(val.clone())),
        TomlValue::Integer(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Float(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Boolean(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Datetime(val) => Ok(toml_edit::Value::from(*val)),
        TomlValue::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_value_to_value(item)?);
            }
            Ok(toml_edit::Value::Array(array))
        }
        TomlValue::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, val) in table {
                inline.insert(key, toml_value_to_value(val)?);
            }
            Ok(toml_edit::Value::InlineTable(inline))
        }
    }
}

fn paths_match(expected: impl AsRef<Path>, provided: impl AsRef<Path>) -> bool {
    path_utils::paths_match_after_normalization(expected, provided)
}

fn value_at_path<'a>(root: &'a TomlValue, segments: &[String]) -> Option<&'a TomlValue> {
    let mut current = root;
    for segment in segments {
        match current {
            TomlValue::Table(table) => {
                current = table.get(segment)?;
            }
            TomlValue::Array(items) => {
                let idx = segment.parse::<i64>().ok()?;
                let idx = usize::try_from(idx).ok()?;
                current = items.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn override_message(layer: &ConfigLayerSource) -> String {
    match layer {
        ConfigLayerSource::Mdm { domain, key: _ } => {
            format!("Overridden by managed policy (MDM): {domain}")
        }
        ConfigLayerSource::System { file } => {
            format!("Overridden by managed config (system): {}", file.display())
        }
        ConfigLayerSource::EnterpriseManaged { id: _, name } => {
            format!("Overridden by enterprise-managed config: {name}")
        }
        ConfigLayerSource::Project { dot_codex_folder } => format!(
            "Overridden by project config: {}/{CONFIG_TOML_FILE}",
            dot_codex_folder.display(),
        ),
        ConfigLayerSource::SessionFlags => "Overridden by session flags".to_string(),
        ConfigLayerSource::User { file, .. } => {
            format!("Overridden by user config: {}", file.display())
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file } => {
            format!(
                "Overridden by legacy managed_config.toml: {}",
                file.display()
            )
        }
        ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {
            "Overridden by legacy managed configuration from MDM".to_string()
        }
    }
}

fn compute_override_metadata(
    layers: &ConfigLayerStack,
    effective: &TomlValue,
    segments: &[String],
) -> Option<OverriddenMetadata> {
    let user_value = match layers.get_active_user_layer() {
        Some(user_layer) => value_at_path(&user_layer.config, segments),
        None => return None,
    };
    let effective_value = value_at_path(effective, segments);

    if user_value.is_some() && user_value == effective_value {
        return None;
    }

    if user_value.is_none() && effective_value.is_none() {
        return None;
    }

    let overriding_layer = find_effective_layer(layers, segments)?;
    let message = override_message(&overriding_layer.name);

    Some(OverriddenMetadata {
        message,
        overriding_layer: config_layer_metadata_to_api(overriding_layer),
        effective_value: effective_value
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or(JsonValue::Null),
    })
}

fn first_overridden_edit(
    layers: &ConfigLayerStack,
    effective: &TomlValue,
    edits: &[Vec<String>],
) -> Option<OverriddenMetadata> {
    for segments in edits {
        if let Some(meta) = compute_override_metadata(layers, effective, segments) {
            return Some(meta);
        }
    }
    None
}

fn find_effective_layer(
    layers: &ConfigLayerStack,
    segments: &[String],
) -> Option<ConfigLayerMetadata> {
    for layer in layers.layers_high_to_low() {
        if let Some(meta) = value_at_path(&layer.config, segments).map(|_| layer.metadata()) {
            return Some(meta);
        }
    }

    None
}

#[cfg(test)]
#[path = "config_manager_service_tests.rs"]
mod tests;
