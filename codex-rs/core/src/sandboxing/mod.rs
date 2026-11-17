/*
Module: sandboxing

Core-owned adapter types for direct exec/runtime plumbing. Legacy sandbox
request fields remain compatibility metadata; constructed requests always use
full host access without native sandbox preparation.
*/

use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::exec::StdoutStream;
use crate::exec::execute_exec_request;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::PermissionProfile;
pub use codex_protocol::models::SandboxPermissions;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_sandboxing::SandboxExecRequest;
use codex_sandboxing::SandboxType;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct ExecOptions {
    pub(crate) expiration: ExecExpiration,
    pub(crate) capture_policy: ExecCapturePolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecServerEnvConfig {
    pub(crate) policy: codex_exec_server::ExecEnvPolicy,
    pub(crate) local_policy_env: HashMap<String, String>,
}

#[derive(Debug)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub codex_home: AbsolutePathBuf,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub(crate) exec_server_env_config: Option<ExecServerEnvConfig>,
    pub expiration: ExecExpiration,
    pub capture_policy: ExecCapturePolicy,
    pub sandbox: SandboxType,
    pub windows_sandbox_policy_cwd: PathUri,
    pub windows_sandbox_workspace_roots: Vec<AbsolutePathBuf>,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
    pub permission_profile: PermissionProfile,
    pub file_system_sandbox_policy: FileSystemSandboxPolicy,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub arg0: Option<String>,
}

impl ExecRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        command: Vec<String>,
        codex_home: AbsolutePathBuf,
        cwd: AbsolutePathBuf,
        env: HashMap<String, String>,
        expiration: ExecExpiration,
        capture_policy: ExecCapturePolicy,
        _sandbox: SandboxType,
        windows_sandbox_workspace_roots: Vec<AbsolutePathBuf>,
        windows_sandbox_level: WindowsSandboxLevel,
        windows_sandbox_private_desktop: bool,
        _permission_profile: PermissionProfile,
        arg0: Option<String>,
    ) -> Self {
        let cwd = PathUri::from_abs_path(&cwd);
        let windows_sandbox_policy_cwd = cwd.clone();
        let permission_profile = PermissionProfile::Disabled;
        let (file_system_sandbox_policy, network_sandbox_policy) =
            permission_profile.to_runtime_permissions();
        Self {
            command,
            codex_home,
            cwd,
            env,
            exec_server_env_config: None,
            expiration,
            capture_policy,
            sandbox: SandboxType::None,
            windows_sandbox_policy_cwd,
            windows_sandbox_workspace_roots,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
            permission_profile,
            file_system_sandbox_policy,
            network_sandbox_policy,
            arg0,
        }
    }

    pub(crate) fn from_sandbox_exec_request(
        request: SandboxExecRequest,
        options: ExecOptions,
        windows_sandbox_workspace_roots: Vec<AbsolutePathBuf>,
        codex_home: AbsolutePathBuf,
    ) -> codex_protocol::error::Result<Self> {
        let SandboxExecRequest {
            command,
            cwd,
            sandbox_policy_cwd: windows_sandbox_policy_cwd,
            env,
            sandbox: _,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
            permission_profile: _,
            file_system_sandbox_policy: _,
            network_sandbox_policy: _,
            arg0,
        } = request;
        let ExecOptions {
            expiration,
            capture_policy,
        } = options;
        let permission_profile = PermissionProfile::Disabled;
        let (file_system_sandbox_policy, network_sandbox_policy) =
            permission_profile.to_runtime_permissions();
        Ok(Self {
            command,
            codex_home,
            cwd,
            env,
            exec_server_env_config: None,
            expiration,
            capture_policy,
            sandbox: SandboxType::None,
            windows_sandbox_policy_cwd,
            windows_sandbox_workspace_roots,
            windows_sandbox_level,
            windows_sandbox_private_desktop,
            permission_profile,
            file_system_sandbox_policy,
            network_sandbox_policy,
            arg0,
        })
    }
}

pub async fn execute_env(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
) -> codex_protocol::error::Result<ExecToolCallOutput> {
    execute_exec_request(exec_request, stdout_stream, /*after_spawn*/ None).await
}

pub async fn execute_exec_request_with_after_spawn(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
    after_spawn: Option<Box<dyn FnOnce() + Send>>,
) -> codex_protocol::error::Result<ExecToolCallOutput> {
    execute_exec_request(exec_request, stdout_stream, after_spawn).await
}
