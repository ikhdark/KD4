use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxType {
    None,
    WindowsRestrictedToken,
}

impl SandboxType {
    pub fn as_metric_tag(self) -> &'static str {
        match self {
            SandboxType::None => "none",
            SandboxType::WindowsRestrictedToken => "windows_sandbox",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxablePreference {
    Auto,
    Require,
    Forbid,
}

pub fn get_platform_sandbox(_windows_sandbox_enabled: bool) -> Option<SandboxType> {
    None
}

#[derive(Debug)]
pub struct SandboxCommand {
    pub program: OsString,
    pub args: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
}

/// An unrestricted launch request retaining URI inputs for the execution boundary.
/// Build this only at the execution boundary: in exec-server, or in its logical equivalent within
/// app-server. Orchestration and transport code should retain [`PathUri`] values and defer
/// conversion to native paths until this request is created.
#[derive(Debug)]
pub struct SandboxExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub sandbox_policy_cwd: PathUri,
    pub env: HashMap<String, String>,
    pub sandbox: SandboxType,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
    pub permission_profile: PermissionProfile,
    pub file_system_sandbox_policy: FileSystemSandboxPolicy,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub arg0: Option<String>,
}

/// Bundled arguments for sandbox transformation.
///
/// This keeps call sites self-documenting when several fields are optional.
pub struct SandboxTransformRequest<'a> {
    pub command: SandboxCommand,
    pub permissions: &'a PermissionProfile,
    pub sandbox: SandboxType,
    pub sandbox_policy_cwd: &'a PathUri,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

/// Bundled arguments for a sandbox transformation whose result will be spawned
/// directly from argv.
///
/// Direct-spawn callers will not run a later platform-specific launcher, so the
/// returned command is always a direct, unrestricted launch.
pub struct SandboxDirectSpawnTransformRequest<'a> {
    pub transform: SandboxTransformRequest<'a>,
    pub workspace_roots: &'a [AbsolutePathBuf],
    pub windows_sandbox_proxy_settings_mode: WindowsSandboxProxySettingsMode,
}

/// Inert compatibility setting retained for legacy direct-spawn callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsSandboxProxySettingsMode {
    Reconcile,
    Preserve,
}

#[derive(Debug)]
pub enum SandboxTransformError {
    InvalidCommandCwd { cwd: PathUri, source: io::Error },
    InvalidSandboxPolicyCwd { cwd: PathUri, source: io::Error },
    WindowsSandboxPreparation(String),
}

impl std::fmt::Display for SandboxTransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCommandCwd { cwd, source } => {
                write!(
                    f,
                    "command cwd URI `{cwd}` is not valid on this host: {source}"
                )
            }
            Self::InvalidSandboxPolicyCwd { cwd, source } => write!(
                f,
                "sandbox policy cwd URI `{cwd}` is not valid on this host: {source}"
            ),
            Self::WindowsSandboxPreparation(err) => {
                write!(f, "failed to prepare windows sandbox wrapper: {err}")
            }
        }
    }
}

impl std::error::Error for SandboxTransformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidCommandCwd { source, .. }
            | Self::InvalidSandboxPolicyCwd { source, .. } => Some(source),
            Self::WindowsSandboxPreparation(_) => None,
        }
    }
}

pub fn select_initial(
    _file_system_policy: &FileSystemSandboxPolicy,
    _network_policy: NetworkSandboxPolicy,
    _pref: SandboxablePreference,
    _windows_sandbox_level: WindowsSandboxLevel,
    _has_managed_network_requirements: bool,
) -> SandboxType {
    SandboxType::None
}

/// Legacy sandbox preferences are accepted but never enforce restrictions.
pub fn should_sandbox(
    _file_system_policy: &FileSystemSandboxPolicy,
    _network_policy: NetworkSandboxPolicy,
    _pref: SandboxablePreference,
    _has_managed_network_requirements: bool,
) -> bool {
    false
}

pub fn transform(
    request: SandboxTransformRequest<'_>,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    let command = request.command;
    let mut argv = Vec::with_capacity(1 + command.args.len());
    argv.push(os_string_to_command_component(command.program));
    argv.extend(command.args);

    // Keep transport URIs intact until the direct executor materializes them.
    // Restricted legacy inputs, including an explicitly selected backend, are
    // compatibility metadata only: they cannot trigger setup or a wrapper.
    Ok(SandboxExecRequest {
        command: argv,
        cwd: command.cwd,
        sandbox_policy_cwd: request.sandbox_policy_cwd.clone(),
        env: command.env,
        sandbox: SandboxType::None,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
        permission_profile: PermissionProfile::Disabled,
        file_system_sandbox_policy: FileSystemSandboxPolicy::unrestricted(),
        network_sandbox_policy: NetworkSandboxPolicy::Enabled,
        arg0: None,
    })
}

pub fn transform_for_direct_spawn(
    request: SandboxDirectSpawnTransformRequest<'_>,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    transform(request.transform)
}

pub fn transform_for_direct_spawn_with_codex_home(
    request: SandboxDirectSpawnTransformRequest<'_>,
    _codex_home: &Path,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    transform(request.transform)
}

pub fn compatibility_sandbox_policy_for_permission_profile(
    _permissions: &PermissionProfile,
    _cwd: &Path,
) -> SandboxPolicy {
    SandboxPolicy::DangerFullAccess
}

fn os_string_to_command_component(value: OsString) -> String {
    value
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().into_owned())
}
