use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_network_proxy::CUSTOM_CA_ENV_KEYS;
use codex_network_proxy::is_managed_mitm_ca_trust_bundle_path;
use codex_protocol::models::PermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxDirectSpawnTransformRequest;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::with_managed_mitm_ca_readable_root;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;

#[cfg(target_os = "windows")]
use codex_protocol::config_types::WindowsSandboxLevel;
#[cfg(target_os = "windows")]
use codex_sandboxing::WindowsSandboxFilesystemOverrides;
#[cfg(target_os = "windows")]
use codex_sandboxing::resolve_windows_elevated_filesystem_overrides;
#[cfg(target_os = "windows")]
use codex_sandboxing::resolve_windows_restricted_token_filesystem_overrides;
#[cfg(target_os = "windows")]
use codex_sandboxing::windows_sandbox_uses_elevated_backend;

use crate::ExecServerRuntimePaths;
use crate::protocol::ExecParams;
use crate::rpc::invalid_params;

pub(crate) struct PreparedExecRequest {
    pub(crate) command: Vec<String>,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) env: HashMap<String, String>,
    pub(crate) arg0: Option<String>,
    pub(crate) sandbox: SandboxType,
    #[cfg(target_os = "windows")]
    windows_sandbox: Option<PreparedWindowsSandbox>,
}

#[cfg(target_os = "windows")]
struct PreparedWindowsSandbox {
    permission_profile: PermissionProfile,
    workspace_roots: Vec<AbsolutePathBuf>,
    windows_sandbox_level: WindowsSandboxLevel,
    proxy_enforced: bool,
    filesystem_overrides: Option<WindowsSandboxFilesystemOverrides>,
    use_private_desktop: bool,
}

impl PreparedExecRequest {
    pub(crate) async fn spawn(self, tty: bool, pipe_stdin: bool) -> Result<SpawnedProcess, String> {
        #[cfg(target_os = "windows")]
        {
            let mut request = self;
            if let Some(windows_sandbox) = request.windows_sandbox.take() {
                return spawn_windows_sandbox(request, windows_sandbox, tty, pipe_stdin).await;
            }
            request.spawn_direct(tty, pipe_stdin).await
        }

        #[cfg(not(target_os = "windows"))]
        self.spawn_direct(tty, pipe_stdin).await
    }

    async fn spawn_direct(self, tty: bool, pipe_stdin: bool) -> Result<SpawnedProcess, String> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| "argv must not be empty".to_string())?;
        let spawned = if tty {
            codex_utils_pty::spawn_pty_process(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
                TerminalSize::default(),
            )
            .await
        } else if pipe_stdin {
            codex_utils_pty::spawn_pipe_process(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
            )
            .await
        } else {
            codex_utils_pty::spawn_pipe_process_no_stdin(
                program,
                args,
                self.cwd.as_path(),
                &self.env,
                &self.arg0,
            )
            .await
        };
        spawned.map_err(|err| err.to_string())
    }
}

#[cfg(target_os = "windows")]
async fn spawn_windows_sandbox(
    mut request: PreparedExecRequest,
    windows_sandbox: PreparedWindowsSandbox,
    tty: bool,
    pipe_stdin: bool,
) -> Result<SpawnedProcess, String> {
    // Match the existing Windows launch semantics: pipe processes ignore arg0, while the
    // portable PTY backend treats arg0 as the executable name rather than a distinct argv[0].
    request.command = windows_sandbox_command(request.command, request.arg0.take(), tty)?;

    let codex_home = codex_utils_home_dir::find_codex_home().map_err(|err| err.to_string())?;
    let empty_paths: &[AbsolutePathBuf] = &[];
    let read_roots_override = windows_sandbox
        .filesystem_overrides
        .as_ref()
        .and_then(|overrides| overrides.read_roots_override.as_deref());
    let read_roots_include_platform_defaults = windows_sandbox
        .filesystem_overrides
        .as_ref()
        .is_some_and(|overrides| overrides.read_roots_include_platform_defaults);
    let write_roots_override = windows_sandbox
        .filesystem_overrides
        .as_ref()
        .and_then(|overrides| overrides.write_roots_override.as_deref());
    let deny_read_paths_override = windows_sandbox
        .filesystem_overrides
        .as_ref()
        .map_or(empty_paths, |overrides| {
            overrides.additional_deny_read_paths.as_slice()
        });
    let deny_write_paths_override = windows_sandbox
        .filesystem_overrides
        .as_ref()
        .map_or(empty_paths, |overrides| {
            overrides.additional_deny_write_paths.as_slice()
        });

    codex_windows_sandbox::spawn_windows_sandbox_session_for_level(
        codex_windows_sandbox::WindowsSandboxSessionRequest {
            permission_profile: &windows_sandbox.permission_profile,
            workspace_roots: windows_sandbox.workspace_roots.as_slice(),
            codex_home: codex_home.as_path(),
            command: request.command,
            cwd: request.cwd.as_path(),
            env_map: request.env,
            windows_sandbox_level: windows_sandbox.windows_sandbox_level,
            proxy_enforced: windows_sandbox.proxy_enforced,
            proxy_settings_mode: codex_windows_sandbox::WindowsSandboxProxySettingsMode::Reconcile,
            timeout_ms: None,
            read_roots_override,
            read_roots_include_platform_defaults,
            write_roots_override,
            deny_read_paths_override,
            deny_write_paths_override,
            tty,
            stdin_open: windows_sandbox_stdin_open(tty, pipe_stdin),
            use_private_desktop: windows_sandbox.use_private_desktop,
        },
    )
    .await
    .map_err(|err| err.to_string())
}

#[cfg(target_os = "windows")]
fn windows_sandbox_command(
    mut command: Vec<String>,
    arg0: Option<String>,
    tty: bool,
) -> Result<Vec<String>, String> {
    if tty && let Some(arg0) = arg0 {
        let program = command
            .first_mut()
            .ok_or_else(|| "argv must not be empty".to_string())?;
        *program = arg0;
    }
    Ok(command)
}

#[cfg(target_os = "windows")]
fn windows_sandbox_stdin_open(tty: bool, pipe_stdin: bool) -> bool {
    tty || pipe_stdin
}

pub(crate) fn prepare_exec_request(
    params: &ExecParams,
    env: HashMap<String, String>,
    runtime_paths: Option<&ExecServerRuntimePaths>,
) -> Result<PreparedExecRequest, JSONRPCErrorError> {
    let Some(sandbox_context) = params.sandbox.as_ref() else {
        return Ok(PreparedExecRequest {
            command: params.argv.clone(),
            cwd: native_path(&params.cwd, "cwd")?,
            env,
            arg0: params.arg0.clone(),
            sandbox: SandboxType::None,
            #[cfg(target_os = "windows")]
            windows_sandbox: None,
        });
    };
    let runtime_paths = runtime_paths
        .ok_or_else(|| invalid_params("sandbox runtime paths are not configured".to_string()))?;
    // TODO(jif): Transport permissions before orchestrator-local paths are materialized,
    // then resolve executor-local helper and workspace paths here.
    let permissions: PermissionProfile = sandbox_context
        .permissions
        .clone()
        .try_into()
        .map_err(|err| invalid_params(format!("invalid sandbox permission path URI: {err}")))?;
    let sandbox_policy_cwd = sandbox_context.cwd.as_ref().unwrap_or(&params.cwd);
    let native_sandbox_policy_cwd = native_path(sandbox_policy_cwd, "sandbox cwd")?;
    let native_workspace_roots = sandbox_context
        .workspace_roots
        .iter()
        .map(|root| native_path(root, "sandbox workspace root"))
        .collect::<Result<Vec<_>, _>>()?;
    let workspace_roots = if native_workspace_roots.is_empty() {
        std::slice::from_ref(&native_sandbox_policy_cwd)
    } else {
        native_workspace_roots.as_slice()
    };
    let permissions = permissions.materialize_project_roots_with_workspace_roots(workspace_roots);
    let managed_mitm_ca_trust_bundle_path = params.managed_network.as_ref().and_then(|_| {
        CUSTOM_CA_ENV_KEYS.iter().find_map(|key| {
            let path = env.get(*key)?;
            if !is_managed_mitm_ca_trust_bundle_path(path) {
                return None;
            }
            AbsolutePathBuf::from_absolute_path(path).ok()
        })
    });
    let permissions = with_managed_mitm_ca_readable_root(
        permissions,
        managed_mitm_ca_trust_bundle_path.as_ref(),
        native_sandbox_policy_cwd.as_path(),
    );
    let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
    let sandbox_manager = SandboxManager::new();
    let sandbox = sandbox_manager.select_initial(
        &file_system_policy,
        network_policy,
        SandboxablePreference::Require,
        sandbox_context.windows_sandbox_level,
        params.enforce_managed_network,
    );
    if sandbox == SandboxType::None {
        return Err(invalid_params(
            "sandbox intent cannot be enforced on this executor".to_string(),
        ));
    }
    let (program, args) = params
        .argv
        .split_first()
        .ok_or_else(|| invalid_params("argv must not be empty".to_string()))?;

    #[cfg(target_os = "windows")]
    if sandbox == SandboxType::WindowsRestrictedToken {
        let request = sandbox_manager
            .transform(SandboxTransformRequest {
                command: SandboxCommand {
                    program: program.into(),
                    args: args.to_vec(),
                    cwd: params.cwd.clone(),
                    env,
                    managed_network: params.managed_network.clone(),
                    additional_permissions: None,
                },
                permissions: &permissions,
                sandbox,
                enforce_managed_network: params.enforce_managed_network,
                environment_id: None,
                network: None,
                sandbox_policy_cwd,
                codex_linux_sandbox_exe: runtime_paths.codex_linux_sandbox_exe.as_deref(),
                use_legacy_landlock: sandbox_context.use_legacy_landlock,
                windows_sandbox_level: sandbox_context.windows_sandbox_level,
                windows_sandbox_private_desktop: sandbox_context.windows_sandbox_private_desktop,
            })
            .map_err(|err| invalid_params(format!("failed to prepare process sandbox: {err}")))?;
        let proxy_enforced = params.enforce_managed_network;
        let use_elevated =
            windows_sandbox_uses_elevated_backend(request.windows_sandbox_level, proxy_enforced);
        let filesystem_overrides = if use_elevated {
            resolve_windows_elevated_filesystem_overrides(
                request.sandbox,
                &request.permission_profile,
                &native_sandbox_policy_cwd,
                use_elevated,
            )
        } else {
            resolve_windows_restricted_token_filesystem_overrides(
                request.sandbox,
                &request.permission_profile,
                &native_sandbox_policy_cwd,
                request.windows_sandbox_level,
            )
        }
        .map_err(|err| invalid_params(format!("failed to prepare process sandbox: {err}")))?;
        return Ok(PreparedExecRequest {
            command: request.command,
            cwd: native_path(&request.cwd, "cwd")?,
            env: request.env,
            arg0: params.arg0.clone(),
            sandbox: request.sandbox,
            windows_sandbox: Some(PreparedWindowsSandbox {
                permission_profile: request.permission_profile,
                workspace_roots: workspace_roots.to_vec(),
                windows_sandbox_level: request.windows_sandbox_level,
                proxy_enforced,
                filesystem_overrides,
                use_private_desktop: request.windows_sandbox_private_desktop,
            }),
        });
    }

    #[cfg(not(target_os = "windows"))]
    if sandbox == SandboxType::WindowsRestrictedToken {
        return Err(invalid_params(
            "windows sandbox selected on a non-Windows executor".to_string(),
        ));
    }

    let request = sandbox_manager
        .transform_for_direct_spawn(SandboxDirectSpawnTransformRequest {
            workspace_roots,
            windows_sandbox_proxy_settings_mode:
                codex_sandboxing::WindowsSandboxProxySettingsMode::Reconcile,
            transform: SandboxTransformRequest {
                // TODO(jif): Preserve params.arg0 for the inner command across the sandbox
                // wrapper, or reject sandboxed requests with a custom arg0.
                command: SandboxCommand {
                    program: program.into(),
                    args: args.to_vec(),
                    cwd: params.cwd.clone(),
                    env,
                    managed_network: params.managed_network.clone(),
                    additional_permissions: None,
                },
                permissions: &permissions,
                sandbox,
                enforce_managed_network: params.enforce_managed_network,
                environment_id: None,
                network: None,
                sandbox_policy_cwd,
                codex_linux_sandbox_exe: runtime_paths.codex_linux_sandbox_exe.as_deref(),
                use_legacy_landlock: sandbox_context.use_legacy_landlock,
                windows_sandbox_level: sandbox_context.windows_sandbox_level,
                windows_sandbox_private_desktop: sandbox_context.windows_sandbox_private_desktop,
            },
        })
        .map_err(|err| invalid_params(format!("failed to prepare process sandbox: {err}")))?;
    Ok(PreparedExecRequest {
        command: request.command,
        cwd: native_path(&request.cwd, "cwd")?,
        env: request.env,
        arg0: request.arg0,
        sandbox: request.sandbox,
        #[cfg(target_os = "windows")]
        windows_sandbox: None,
    })
}

fn native_path(path: &PathUri, label: &str) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    path.to_abs_path().map_err(|err| {
        invalid_params(format!(
            "{label} URI `{path}` is not valid on this exec-server host: {err}"
        ))
    })
}

#[cfg(test)]
#[path = "process_sandbox_tests.rs"]
mod tests;
