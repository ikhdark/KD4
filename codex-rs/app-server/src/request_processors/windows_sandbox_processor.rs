use super::*;

#[derive(Clone)]
pub(crate) struct WindowsSandboxRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    config: Arc<Config>,
    config_manager: ConfigManager,
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
        }
    }

    pub(crate) async fn windows_sandbox_readiness(
        &self,
    ) -> Result<WindowsSandboxReadinessResponse, JSONRPCErrorError> {
        Ok(determine_windows_sandbox_readiness(&self.config))
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
            .map_err(|err| config_load_error(&err))?;
        let permission_profile = config.permissions.effective_permission_profile();
        let workspace_roots = config.effective_workspace_roots();
        let codex_home = config.codex_home.to_path_buf();
        let read_root = PathBuf::from(params.path);
        let canonical_path = tokio::task::spawn_blocking(move || {
            codex_core::grant_read_root_non_elevated(
                &permission_profile,
                &workspace_roots,
                &command_cwd,
                &std::env::vars().collect(),
                &codex_home,
                &read_root,
            )
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
            .map_err(|err| config_load_error(&err))?;
        let setup_mode = resolve_allowed_windows_sandbox_setup_mode(
            config.config_layer_stack.requirements(),
            params.mode,
        )?;

        self.outgoing
            .send_response(
                request_id.clone(),
                WindowsSandboxSetupStartResponse { started: true },
            )
            .await;

        let outgoing = Arc::clone(&self.outgoing);
        let connection_id = request_id.connection_id;
        let permission_profile = inline_permission_profile
            .unwrap_or_else(|| config.permissions.effective_permission_profile());

        tokio::spawn(async move {
            let setup_request = WindowsSandboxSetupRequest {
                mode: setup_mode,
                permission_profile,
                workspace_roots: config.effective_workspace_roots(),
                command_cwd,
                env_map: std::env::vars().collect(),
                codex_home: config.codex_home.to_path_buf(),
            };
            let setup_result =
                codex_core::windows_sandbox::run_windows_sandbox_setup(setup_request).await;
            let notification = WindowsSandboxSetupCompletedNotification {
                mode: match setup_mode {
                    CoreWindowsSandboxSetupMode::Elevated => WindowsSandboxSetupMode::Elevated,
                    CoreWindowsSandboxSetupMode::Unelevated => WindowsSandboxSetupMode::Unelevated,
                },
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
    let (setup_mode, config_mode) = match requested_mode {
        WindowsSandboxSetupMode::Elevated => (
            CoreWindowsSandboxSetupMode::Elevated,
            codex_config::types::WindowsSandboxModeToml::Elevated,
        ),
        WindowsSandboxSetupMode::Unelevated => (
            CoreWindowsSandboxSetupMode::Unelevated,
            codex_config::types::WindowsSandboxModeToml::Unelevated,
        ),
    };
    requirements
        .windows_sandbox_mode
        .can_set(&Some(config_mode))
        .map_err(|err| invalid_request(format!("invalid Windows sandbox setup mode: {err}")))?;
    Ok(setup_mode)
}

fn determine_windows_sandbox_readiness(config: &Config) -> WindowsSandboxReadinessResponse {
    determine_windows_sandbox_readiness_for_platform(
        cfg!(windows),
        WindowsSandboxLevel::from_config(config),
        sandbox_setup_is_complete(config.codex_home.as_path()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_code::INVALID_REQUEST_ERROR_CODE;
    use codex_config::ConfigRequirements;
    use codex_config::Constrained;
    use codex_config::ConstrainedWithSource;
    use codex_config::types::WindowsSandboxModeToml;

    #[test]
    fn resolve_allowed_windows_sandbox_setup_mode_rejects_disallowed_mode() {
        let requirements = ConfigRequirements {
            windows_sandbox_mode: ConstrainedWithSource::new(
                Constrained::allow_only(Some(WindowsSandboxModeToml::Elevated)),
                /*source*/ None,
            ),
            ..Default::default()
        };

        let err = resolve_allowed_windows_sandbox_setup_mode(
            &requirements,
            WindowsSandboxSetupMode::Unelevated,
        )
        .expect_err("unelevated setup should be rejected");

        assert_eq!(err.code, INVALID_REQUEST_ERROR_CODE);
        assert!(
            err.message.contains("invalid Windows sandbox setup mode"),
            "{err:?}"
        );
    }

    #[test]
    fn setup_profile_rejects_named_and_inline_profile_together() {
        let params = WindowsSandboxSetupStartParams {
            mode: WindowsSandboxSetupMode::Elevated,
            cwd: None,
            permission_profile_id: Some("managed-default".to_string()),
            permission_profile: Some(codex_protocol::models::PermissionProfile::read_only()),
        };

        let err = validate_windows_sandbox_setup_profile(&params)
            .expect_err("setup profile representations must be mutually exclusive");

        assert_eq!(err.code, INVALID_REQUEST_ERROR_CODE);
        assert!(err.message.contains("mutually exclusive"), "{err:?}");
    }

    #[test]
    fn determine_windows_sandbox_readiness_reports_not_configured_when_disabled() {
        let response = determine_windows_sandbox_readiness_from_state(
            WindowsSandboxLevel::Disabled,
            /*sandbox_setup_is_complete*/ false,
        );

        assert_eq!(response.status, WindowsSandboxReadiness::NotConfigured);
    }

    #[test]
    fn determine_windows_sandbox_readiness_reports_ready_for_unelevated_mode() {
        let response = determine_windows_sandbox_readiness_from_state(
            WindowsSandboxLevel::RestrictedToken,
            /*sandbox_setup_is_complete*/ false,
        );

        assert_eq!(response.status, WindowsSandboxReadiness::Ready);
    }

    #[test]
    fn determine_windows_sandbox_readiness_reports_ready_for_complete_elevated_mode() {
        let response = determine_windows_sandbox_readiness_from_state(
            WindowsSandboxLevel::Elevated,
            /*sandbox_setup_is_complete*/ true,
        );

        assert_eq!(response.status, WindowsSandboxReadiness::Ready);
    }

    #[test]
    fn determine_windows_sandbox_readiness_reports_update_required_when_elevated_setup_is_stale() {
        let response = determine_windows_sandbox_readiness_from_state(
            WindowsSandboxLevel::Elevated,
            /*sandbox_setup_is_complete*/ false,
        );

        assert_eq!(response.status, WindowsSandboxReadiness::UpdateRequired);
    }

    #[test]
    fn determine_windows_sandbox_readiness_reports_unsupported_off_windows() {
        let response = determine_windows_sandbox_readiness_for_platform(
            /*is_windows*/ false,
            WindowsSandboxLevel::Elevated,
            /*sandbox_setup_is_complete*/ true,
        );

        assert_eq!(response.status, WindowsSandboxReadiness::Unsupported);
    }
}
