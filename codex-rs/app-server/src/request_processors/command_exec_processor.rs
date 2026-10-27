use super::*;
use crate::connection_rpc_gate::ConnectionRpcGate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommandExecLogMetadata {
    argv_count: usize,
    env_key_count: usize,
    process_id_present: bool,
    tty: bool,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    output_cap_present: bool,
    disable_output_cap: bool,
    disable_timeout: bool,
    timeout_present: bool,
    cwd_present: bool,
    size_present: bool,
    sandbox_policy_present: bool,
    permission_profile_present: bool,
}

fn command_exec_log_metadata(params: &CommandExecParams) -> CommandExecLogMetadata {
    CommandExecLogMetadata {
        argv_count: params.command.len(),
        env_key_count: params.env.as_ref().map_or(0, HashMap::len),
        process_id_present: params.process_id.is_some(),
        tty: params.tty,
        stream_stdin: params.stream_stdin,
        stream_stdout_stderr: params.stream_stdout_stderr,
        output_cap_present: params.output_bytes_cap.is_some(),
        disable_output_cap: params.disable_output_cap,
        disable_timeout: params.disable_timeout,
        timeout_present: params.timeout_ms.is_some(),
        cwd_present: params.cwd.is_some(),
        size_present: params.size.is_some(),
        sandbox_policy_present: params.sandbox_policy.is_some(),
        permission_profile_present: params.permission_profile.is_some(),
    }
}

fn log_command_exec_request(params: &CommandExecParams) {
    let metadata = command_exec_log_metadata(params);
    tracing::debug!(
        rpc.method = "command/exec",
        argv_count = metadata.argv_count,
        env_key_count = metadata.env_key_count,
        process_id_present = metadata.process_id_present,
        tty = metadata.tty,
        stream_stdin = metadata.stream_stdin,
        stream_stdout_stderr = metadata.stream_stdout_stderr,
        output_cap_present = metadata.output_cap_present,
        disable_output_cap = metadata.disable_output_cap,
        disable_timeout = metadata.disable_timeout,
        timeout_present = metadata.timeout_present,
        cwd_present = metadata.cwd_present,
        size_present = metadata.size_present,
        sandbox_policy_present = metadata.sandbox_policy_present,
        permission_profile_present = metadata.permission_profile_present,
        "command request"
    );
}

fn legacy_command_exec_config(
    base_config: &Config,
    cwd: &AbsolutePathBuf,
    sandbox_policy: codex_protocol::protocol::SandboxPolicy,
) -> codex_core::config::ConstraintResult<Config> {
    let mut config = base_config.clone();
    config.cwd = cwd.clone();
    config.set_legacy_sandbox_policy(sandbox_policy)?;
    if matches!(
        config.permissions.permission_profile(),
        codex_protocol::models::PermissionProfile::Disabled
    ) {
    }
    Ok(config)
}

/// Loads the config a `permissionProfile` request runs under.
async fn permission_profile_command_exec_config(
    config_manager: &ConfigManager,
    base_cwd: &AbsolutePathBuf,
    cwd: &AbsolutePathBuf,
    permission_profile: String,
) -> Result<Config, JSONRPCErrorError> {
    let overrides = ConfigOverrides {
        cwd: Some(cwd.to_path_buf()),
        default_permissions: Some(permission_profile),
        ..Default::default()
    };
    let config = config_manager
        .load_for_cwd(
            /*request_overrides*/ None,
            overrides,
            Some(base_cwd.to_path_buf()),
        )
        .await
        .map_err(|err| invalid_request(format!("invalid permission profile: {err}")))?;
    if let Some(warning) = config
        .startup_warnings
        .iter()
        .find(|warning| warning.contains("Configured value for `permission_profile` is disallowed"))
    {
        return Err(invalid_request(format!(
            "invalid permission profile: {warning}"
        )));
    }
    Ok(config)
}

#[derive(Clone)]
pub(crate) struct CommandExecRequestProcessor {
    config: Arc<Config>,
    outgoing: Arc<OutgoingMessageSender>,
    config_manager: ConfigManager,
    environment_manager: Arc<EnvironmentManager>,
    command_exec_manager: CommandExecManager,
}

impl CommandExecRequestProcessor {
    pub(crate) fn new(
        config: Arc<Config>,
        outgoing: Arc<OutgoingMessageSender>,
        config_manager: ConfigManager,
        environment_manager: Arc<EnvironmentManager>,
    ) -> Self {
        Self {
            config,
            outgoing,
            config_manager,
            environment_manager,
            command_exec_manager: CommandExecManager::default(),
        }
    }

    pub(crate) async fn one_off_command_exec(
        &self,
        request_id: &ConnectionRequestId,
        params: CommandExecParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.require_local_environment()?;
        self.exec_one_off_command_inner(request_id.clone(), params, rpc_gate)
            .await
            .map(|()| None)
    }

    pub(crate) async fn command_exec_write(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecWriteParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.command_exec_manager
            .write_with_gate(Arc::clone(&self.outgoing), request_id, params, rpc_gate)
            .await
            .map(|()| None)
    }

    pub(crate) async fn command_exec_resize(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecResizeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.command_exec_manager
            .resize(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn command_exec_terminate(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecTerminateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.command_exec_manager
            .terminate(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.command_exec_manager
            .connection_closed(connection_id)
            .await;
    }

    pub(crate) async fn drain_run_tasks(&self) {
        self.command_exec_manager.drain_run_tasks().await;
    }

    fn require_local_environment(&self) -> Result<(), JSONRPCErrorError> {
        self.environment_manager
            .try_local_environment()
            .is_some()
            .then_some(())
            .ok_or_else(|| internal_error("local environment is not configured"))
    }

    async fn exec_one_off_command_inner(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        log_command_exec_request(&params);

        let request = request_id.clone();

        crate::command_exec::validate_command_argv(&params.command)?;

        let CommandExecParams {
            command,
            process_id,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            disable_output_cap,
            disable_timeout,
            timeout_ms,
            cwd,
            env: env_overrides,
            size,
            sandbox_policy,
            permission_profile,
        } = params;
        if sandbox_policy.is_some() && permission_profile.is_some() {
            return Err(invalid_request(
                "`permissionProfile` cannot be combined with `sandboxPolicy`",
            ));
        }

        if size.is_some() && !tty {
            return Err(invalid_params("command/exec size requires tty: true"));
        }
        let size = size
            .map(|size| {
                crate::command_exec::terminal_size_from_protocol(size.into_inner(), "command/exec")
            })
            .transpose()?;

        if disable_output_cap && output_bytes_cap.is_some() {
            return Err(invalid_params(
                "command/exec cannot set both outputBytesCap and disableOutputCap",
            ));
        }

        if disable_timeout && timeout_ms.is_some() {
            return Err(invalid_params(
                "command/exec cannot set both timeoutMs and disableTimeout",
            ));
        }

        let cwd = cwd.map_or_else(|| self.config.cwd.clone(), |cwd| self.config.cwd.join(cwd));
        let mut env = create_env(
            &self.config.permissions.shell_environment_policy,
            /*thread_id*/ None,
        );
        if let Some(env_overrides) = env_overrides {
            #[cfg(windows)]
            super::process_exec_processor::validate_environment_override_keys(
                &env_overrides,
                "command/exec",
            )?;
            for (key, value) in env_overrides {
                #[cfg(windows)]
                env.retain(|inherited_key, _| !inherited_key.eq_ignore_ascii_case(&key));
                match value {
                    Some(value) => {
                        env.insert(key, value);
                    }
                    None => {
                        env.remove(&key);
                    }
                }
            }
        }
        let timeout_ms = match timeout_ms {
            Some(timeout_ms) => match u64::try_from(timeout_ms) {
                Ok(timeout_ms) => Some(timeout_ms),
                Err(_) => {
                    return Err(invalid_params(format!(
                        "command/exec timeoutMs must be non-negative, got {timeout_ms}"
                    )));
                }
            },
            None => None,
        };
        let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
        let output_bytes_cap = if disable_output_cap {
            None
        } else {
            Some(output_bytes_cap.unwrap_or(DEFAULT_OUTPUT_BYTES_CAP))
        };
        let expiration = if disable_timeout {
            ExecExpiration::Cancellation(CancellationToken::new())
        } else {
            match timeout_ms {
                Some(timeout_ms) => timeout_ms.into(),
                None => ExecExpiration::DefaultTimeout,
            }
        };
        let capture_policy = if disable_output_cap {
            ExecCapturePolicy::FullBuffer
        } else {
            ExecCapturePolicy::ShellTool
        };
        let sandbox_cwd = if permission_profile.is_some() || sandbox_policy.is_some() {
            cwd.clone()
        } else {
            self.config.cwd.clone()
        };
        let (
            effective_permission_profile,
            windows_sandbox_workspace_roots,
        ) = if let Some(permission_profile) = permission_profile {
            let config = permission_profile_command_exec_config(
                &self.config_manager,
                &self.config.cwd,
                &cwd,
                permission_profile,
            )
            .await?;
            (
                config.permissions.effective_permission_profile(),
                config.effective_workspace_roots(),
            )
        } else if let Some(policy) = sandbox_policy.map(|policy| policy.to_core()) {
            let config = legacy_command_exec_config(&self.config, &sandbox_cwd, policy)
                .map_err(|err| invalid_request(format!("invalid sandbox policy: {err}")))?;
            (
                config.permissions.effective_permission_profile(),
                config.effective_workspace_roots(),
            )
        } else {
            (
                self.config.permissions.effective_permission_profile(),
                self.config.effective_workspace_roots(),
            )
        };
        let exec_params = ExecParams {
            command,
            codex_home: self.config.codex_home.clone(),
            cwd: cwd.clone(),
            expiration,
            capture_policy,
            env,
            sandbox_permissions: SandboxPermissions::UseDefault,
            windows_sandbox_level,
            windows_sandbox_private_desktop: self
                .config
                .permissions
                .windows_sandbox_private_desktop,
            justification: None,
            arg0: None,
        };

        let outgoing = self.outgoing.clone();
        let request_for_task = request.clone();

        let exec_request = codex_core::exec::build_exec_request_async(
            exec_params,
            &effective_permission_profile,
            &sandbox_cwd,
            windows_sandbox_workspace_roots.as_slice(),
        )
        .await
        .map_err(|err| internal_error(format!("exec failed: {err}")))?;
        self.command_exec_manager
            .start_with_gate(
                StartCommandExecParams {
                    outgoing,
                    request_id: request_for_task,
                    process_id,
                    exec_request,
                    tty,
                    stream_stdin,
                    stream_stdout_stderr,
                    output_bytes_cap,
                    size,
                },
                rpc_gate,
            )
            .await
    }
}

