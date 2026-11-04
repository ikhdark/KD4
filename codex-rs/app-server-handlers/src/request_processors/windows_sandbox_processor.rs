use super::config_errors::ConfigLoadResultExt;
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use super::AbsolutePathBuf;
use super::Arc;
use super::ClientResponsePayload;
use super::Config;
use super::ConfigManager;
use super::ConfigOverrides;
use super::ConnectionRequestId;
use super::CoreWindowsSandboxSetupMode;
use super::JSONRPCErrorError;
use super::OutgoingMessageSender;
use super::Path;
use super::PathBuf;
use super::Result;
use super::ServerNotification;
use super::WindowsSandboxGrantReadRootParams;
use super::WindowsSandboxGrantReadRootResponse;
use super::WindowsSandboxLevel;
use super::WindowsSandboxReadiness;
use super::WindowsSandboxReadinessResponse;
use super::WindowsSandboxSetupCompletedNotification;
use super::WindowsSandboxSetupMode;
use super::WindowsSandboxSetupRequest;
use super::WindowsSandboxSetupStartParams;
use super::WindowsSandboxSetupStartResponse;
use super::internal_error;
use super::invalid_request;
use super::sandbox_setup_is_complete;

#[derive(Clone)]
pub(crate) struct WindowsSandboxRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    config: Arc<Config>,
    config_manager: ConfigManager,
    setups: Arc<std::sync::Mutex<std::collections::HashSet<crate::outgoing_message::ConnectionId>>>,
}

struct SetupReservation {
    setups: Arc<std::sync::Mutex<std::collections::HashSet<crate::outgoing_message::ConnectionId>>>,
    connection_id: crate::outgoing_message::ConnectionId,
}

impl Drop for SetupReservation {
    fn drop(&mut self) {
        self.setups.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.connection_id);
    }
}

fn sandbox_environment() -> Result<std::collections::HashMap<String, String>, JSONRPCErrorError> {
    std::env::vars_os().map(|(key, value)| {
        let key = key.into_string().map_err(|_| invalid_request("Windows sandbox environment contains a non-Unicode name"))?;
        let value = value.into_string().map_err(|_| invalid_request("Windows sandbox environment contains a non-Unicode value"))?;
        Ok((key, value))
    }).collect()
}

impl WindowsSandboxRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        config: Arc<Config>,
        config_manager: ConfigManager,
    ) -> Self {
        Self {
            outgoing,
            config,
            config_manager,
            setups: Arc::default(),
        }
    }

    pub(crate) async fn windows_sandbox_readiness(
        &self,
    ) -> Result<WindowsSandboxReadinessResponse, JSONRPCErrorError> {
        let config = self
            .config_manager
            .load_latest_config(Some(self.config.cwd.to_path_buf()))
            .await
            .map_config_load_error()?;
        Ok(determine_windows_sandbox_readiness(&config))
    }

    pub(crate) async fn windows_sandbox_grant_read_root(
        &self,
        params: WindowsSandboxGrantReadRootParams,
    ) -> Result<WindowsSandboxGrantReadRootResponse, JSONRPCErrorError> {
        let command_cwd = params
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.cwd.to_path_buf());
        let config = self
            .config_manager
            .load_for_cwd(
                /*request_overrides*/ None,
                ConfigOverrides {
                    cwd: Some(command_cwd.clone()),
                    ..Default::default()
                },
                Some(command_cwd.clone()),
            )
            .await
            .map_config_load_error()?;
        let permission_profile = config.permissions.effective_permission_profile();
        let workspace_roots = config.effective_workspace_roots();
        let codex_home = config.codex_home.to_path_buf();
        let read_root = PathBuf::from(params.path);
        let grant_required = windows_sandbox_read_root_grant_required(&permission_profile)?;
        let env_map = if grant_required { sandbox_environment()? } else { Default::default() };
        let canonical_path = tokio::task::spawn_blocking(move || {
            if grant_required {
                codex_core::grant_read_root_non_elevated(
                    &permission_profile,
                    &workspace_roots,
                    &command_cwd,
                    &env_map,
                    &codex_home,
                    &read_root,
                )
            } else {
                validate_and_canonicalize_read_root(&read_root)
            }
        })
        .await
        .map_err(|err| internal_error(format!("Windows sandbox read-root task failed: {err}")))?
        .map_err(|err| {
            invalid_request(format!("failed to grant Windows sandbox read root: {err}"))
        })?;

        let path = AbsolutePathBuf::try_from(canonical_path).map_err(|err| {
            internal_error(format!(
                "Windows sandbox returned a non-absolute read root: {err}"
            ))
        })?;
        Ok(WindowsSandboxGrantReadRootResponse { path })
    }

    pub(crate) async fn windows_sandbox_setup_start(
        &self,
        request_id: &ConnectionRequestId,
        params: WindowsSandboxSetupStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.windows_sandbox_setup_start_inner(request_id, params)
            .await
            .map(|()| None)
    }

    async fn windows_sandbox_setup_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: WindowsSandboxSetupStartParams,
    ) -> Result<(), JSONRPCErrorError> {
        // Validate requirements before acknowledging setup so callers do not get a
        // `started` response for a Windows sandbox mode that cannot be persisted.
        validate_windows_sandbox_setup_profile(&params)?;
        let _reservation = {
            let mut setups = self.setups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !setups.insert(request_id.connection_id) {
                return Err(invalid_request("Windows sandbox setup is already in progress for this connection"));
            }
            SetupReservation { setups: Arc::clone(&self.setups), connection_id: request_id.connection_id }
        };
        let env_map = sandbox_environment()?;
        let explicit_profile = params.permission_profile_id.is_some();
        let command_cwd = params
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.cwd.to_path_buf());
        let inline_permission_profile = params.permission_profile;
        let config = self
            .config_manager
            .load_for_cwd(
                /*request_overrides*/ None,
                ConfigOverrides {
                    cwd: Some(command_cwd.clone()),
                    default_permissions: params.permission_profile_id,
                    ..Default::default()
                },
                Some(command_cwd.clone()),
            )
            .await
            .map_config_load_error()?;
        let setup_mode = resolve_allowed_windows_sandbox_setup_mode(
            config.config_layer_stack.requirements(),
            params.mode,
        )?;

        if explicit_profile && let Some(warning) = config.startup_warnings.iter().find(|warning| {
            warning.contains("Configured value for `permission_profile` is disallowed")
        }) {
            return Err(invalid_request(format!("invalid Windows sandbox setup profile: {warning}")));
        }
        if let Some(profile) = inline_permission_profile.as_ref() {
            config.permissions.can_set_permission_profile(profile)
                .map_err(|err| invalid_request(format!("invalid Windows sandbox setup profile: {err}")))?;
        }

        self.outgoing
            .send_response(
                request_id.clone(),
                WindowsSandboxSetupStartResponse { started: true },
            )
            .await;

        let outgoing = Arc::clone(&self.outgoing);
        let config_manager = self.config_manager.clone();
        let connection_id = request_id.connection_id;
        let permission_profile = inline_permission_profile
            .unwrap_or_else(|| config.permissions.effective_permission_profile());

        tokio::spawn(async move {
            let _reservation = _reservation;
            let setup_request = WindowsSandboxSetupRequest {
                mode: setup_mode,
                permission_profile,
                workspace_roots: config.effective_workspace_roots(),
                command_cwd,
                env_map,
                codex_home: config.codex_home.to_path_buf(),
            };
            let setup_result = tokio::spawn(async move {
                codex_core::windows_sandbox::run_windows_sandbox_setup(setup_request).await
            }).await.unwrap_or_else(|err| Err(anyhow::anyhow!("Windows sandbox setup task failed: {err}")));
            if setup_result.is_ok() {
                // Setup persists outside ConfigManager. Publish the new mode
                // before a client reacts to the successful completion event.
                config_manager.invalidate_load_cache();
            }
            let notification = WindowsSandboxSetupCompletedNotification {
                mode: setup_mode.into(),
                success: setup_result.is_ok(),
                error: setup_result.err().map(|err| err.to_string()),
            };
            outgoing
                .send_server_notification_to_connections(
                    &[connection_id],
                    ServerNotification::WindowsSandboxSetupCompleted(notification),
                )
                .await;
        });
        Ok(())
    }
}

fn windows_sandbox_read_root_grant_required(
    permission_profile: &codex_protocol::models::PermissionProfile,
) -> Result<bool, JSONRPCErrorError> {
    match permission_profile {
        codex_protocol::models::PermissionProfile::External { .. } => Err(invalid_request(
            "windowsSandbox/grantReadRoot is unavailable for an externally managed sandbox",
        )),
        codex_protocol::models::PermissionProfile::Disabled => Ok(false),
        codex_protocol::models::PermissionProfile::Managed { .. } => Ok(!permission_profile
            .file_system_sandbox_policy()
            .has_full_disk_read_access()),
    }
}

fn validate_and_canonicalize_read_root(read_root: &Path) -> anyhow::Result<PathBuf> {
    if !read_root.is_absolute() {
        anyhow::bail!("path must be absolute: {}", read_root.display());
    }
    let metadata = std::fs::metadata(read_root)?;
    if !metadata.is_dir() {
        anyhow::bail!("path must be a directory: {}", read_root.display());
    }
    Ok(std::fs::canonicalize(read_root)?)
}

fn validate_windows_sandbox_setup_profile(
    params: &WindowsSandboxSetupStartParams,
) -> Result<(), JSONRPCErrorError> {
    if params.permission_profile_id.is_some() && params.permission_profile.is_some() {
        return Err(invalid_request(
            "permissionProfileId and permissionProfile are mutually exclusive",
        ));
    }
    Ok(())
}

/// Resolves the requested API mode after checking that managed requirements allow it.
fn resolve_allowed_windows_sandbox_setup_mode(
    requirements: &codex_config::ConfigRequirements,
    requested_mode: WindowsSandboxSetupMode,
) -> Result<CoreWindowsSandboxSetupMode, JSONRPCErrorError> {
    let setup_mode = requested_mode.to_core();
    let config_mode = match setup_mode {
        CoreWindowsSandboxSetupMode::Elevated => {
            codex_config::types::WindowsSandboxModeToml::Elevated
        }
        CoreWindowsSandboxSetupMode::Unelevated => {
            codex_config::types::WindowsSandboxModeToml::Unelevated
        }
    };
    requirements
        .windows_sandbox_mode
        .can_set(&Some(config_mode))
        .map_err(|err| invalid_request(format!("invalid Windows sandbox setup mode: {err}")))?;
    Ok(setup_mode)
}

fn determine_windows_sandbox_readiness(config: &Config) -> WindowsSandboxReadinessResponse {
    let level = WindowsSandboxLevel::from_config(config);
    determine_windows_sandbox_readiness_for_platform(
        cfg!(windows),
        level,
        matches!(level, WindowsSandboxLevel::Elevated)
            && sandbox_setup_is_complete(config.codex_home.as_path()),
    )
}

fn determine_windows_sandbox_readiness_for_platform(
    is_windows: bool,
    windows_sandbox_level: WindowsSandboxLevel,
    sandbox_setup_is_complete: bool,
) -> WindowsSandboxReadinessResponse {
    if !is_windows {
        return WindowsSandboxReadinessResponse {
            status: WindowsSandboxReadiness::Unsupported,
        };
    }

    determine_windows_sandbox_readiness_from_state(windows_sandbox_level, sandbox_setup_is_complete)
}

fn determine_windows_sandbox_readiness_from_state(
    windows_sandbox_level: WindowsSandboxLevel,
    sandbox_setup_is_complete: bool,
) -> WindowsSandboxReadinessResponse {
    let status = match windows_sandbox_level {
        WindowsSandboxLevel::Disabled => WindowsSandboxReadiness::NotConfigured,
        WindowsSandboxLevel::RestrictedToken => WindowsSandboxReadiness::Ready,
        WindowsSandboxLevel::Elevated => {
            if sandbox_setup_is_complete {
                WindowsSandboxReadiness::Ready
            } else {
                WindowsSandboxReadiness::UpdateRequired
            }
        }
    };

    WindowsSandboxReadinessResponse { status }
}

