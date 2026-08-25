use crate::policy_transforms::effective_permission_profile;
use crate::policy_transforms::should_require_platform_sandbox;
use crate::resolve_windows_elevated_filesystem_overrides;
use crate::resolve_windows_restricted_token_filesystem_overrides;
use crate::windows_sandbox_uses_elevated_backend;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_network_proxy::NetworkProxy;
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

const WINDOWS_SANDBOX_WRAPPER_SETUP_ENV_ALLOWLIST: &[&str] = &["USERNAME", "USERPROFILE"];

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

pub fn get_platform_sandbox(windows_sandbox_enabled: bool) -> Option<SandboxType> {
    if windows_sandbox_enabled {
        Some(SandboxType::WindowsRestrictedToken)
    } else {
        None
    }
}

pub fn with_managed_mitm_ca_readable_root(
    permission_profile: PermissionProfile,
    managed_mitm_ca_trust_bundle_path: Option<&AbsolutePathBuf>,
    sandbox_policy_cwd: &Path,
) -> PermissionProfile {
    let Some(managed_mitm_ca_trust_bundle_path) = managed_mitm_ca_trust_bundle_path else {
        return permission_profile;
    };
    let (file_system_sandbox_policy, network_sandbox_policy) =
        permission_profile.to_runtime_permissions();
    let file_system_sandbox_policy = file_system_sandbox_policy.with_additional_readable_roots(
        sandbox_policy_cwd,
        std::slice::from_ref(managed_mitm_ca_trust_bundle_path),
    );
    PermissionProfile::from_runtime_permissions_with_enforcement(
        permission_profile.enforcement(),
        &file_system_sandbox_policy,
        network_sandbox_policy,
    )
}

#[derive(Debug)]
pub struct SandboxCommand {
    pub program: OsString,
    pub args: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub managed_network: Option<ManagedNetworkSandboxContext>,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
}

/// A host-native launch request produced after [`transform`] validates URI inputs.
/// Build this only at the execution boundary: in exec-server, or in its logical equivalent within
/// app-server. Orchestration and transport code should retain [`PathUri`] values and defer
/// conversion to native paths until this request is created.
#[derive(Debug)]
pub struct SandboxExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub sandbox_policy_cwd: PathUri,
    pub env: HashMap<String, String>,
    pub network: Option<NetworkProxy>,
    pub network_environment_id: Option<String>,
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
    pub enforce_managed_network: bool,
    pub environment_id: Option<&'a str>,
    // TODO(viyatb): Evaluate switching this to Option<Arc<NetworkProxy>>
    // to make shared ownership explicit across runtime/sandbox plumbing.
    pub network: Option<&'a NetworkProxy>,
    pub sandbox_policy_cwd: &'a PathUri,
    pub windows_sandbox_level: WindowsSandboxLevel,
    pub windows_sandbox_private_desktop: bool,
}

/// Bundled arguments for a sandbox transformation whose result will be spawned
/// directly from argv.
///
/// Direct-spawn callers will not run a later platform-specific launcher, so the
/// returned command must encode any sandbox wrapper it needs.
pub struct SandboxDirectSpawnTransformRequest<'a> {
    pub transform: SandboxTransformRequest<'a>,
    pub workspace_roots: &'a [AbsolutePathBuf],
    pub windows_sandbox_proxy_settings_mode: codex_windows_sandbox::WindowsSandboxProxySettingsMode,
}

// TODO(anp): Revisit this preparation type once this module's PathUri migration is complete.
struct PendingSandboxedExecRequest {
    effective_permission_profile: PermissionProfile,
    effective_file_system_policy: FileSystemSandboxPolicy,
    effective_network_policy: NetworkSandboxPolicy,
}

impl PendingSandboxedExecRequest {
    fn new(
        command_cwd: &PathUri,
        sandbox_policy_cwd: &PathUri,
        effective_permission_profile: PermissionProfile,
        managed_mitm_ca_trust_bundle_path: Option<&AbsolutePathBuf>,
    ) -> Result<Self, SandboxTransformError> {
        // TODO(anp): Move PathUri conversion into the platform sandbox implementations.
        let _native_command_cwd = command_cwd.to_abs_path().map_err(|source| {
            SandboxTransformError::InvalidCommandCwd {
                cwd: command_cwd.clone(),
                source,
            }
        })?;
        let native_sandbox_policy_cwd = sandbox_policy_cwd.to_abs_path().map_err(|source| {
            SandboxTransformError::InvalidSandboxPolicyCwd {
                cwd: sandbox_policy_cwd.clone(),
                source,
            }
        })?;
        let effective_permission_profile = with_managed_mitm_ca_readable_root(
            effective_permission_profile,
            managed_mitm_ca_trust_bundle_path,
            native_sandbox_policy_cwd.as_path(),
        );
        let (effective_file_system_policy, effective_network_policy) =
            effective_permission_profile.to_runtime_permissions();
        Ok(Self {
            effective_permission_profile,
            effective_file_system_policy,
            effective_network_policy,
        })
    }
}

#[derive(Debug)]
pub enum SandboxTransformError {
    InvalidCommandCwd { cwd: PathUri, source: io::Error },
    InvalidSandboxPolicyCwd { cwd: PathUri, source: io::Error },
    EnvironmentNetworkProxy(String),
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
            Self::EnvironmentNetworkProxy(err) => {
                write!(f, "failed to prepare environment network proxy: {err}")
            }
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
            Self::EnvironmentNetworkProxy(_) => None,
            Self::WindowsSandboxPreparation(_) => None,
        }
    }
}

pub fn select_initial(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    pref: SandboxablePreference,
    windows_sandbox_level: WindowsSandboxLevel,
    has_managed_network_requirements: bool,
) -> SandboxType {
    if should_sandbox(
        file_system_policy,
        network_policy,
        pref,
        has_managed_network_requirements,
    ) {
        get_platform_sandbox(windows_sandbox_level != WindowsSandboxLevel::Disabled)
            .unwrap_or(SandboxType::None)
    } else {
        SandboxType::None
    }
}

/// Returns whether the request needs a sandbox, independently of whether
/// this host can provide a concrete sandbox implementation.
pub fn should_sandbox(
    file_system_policy: &FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    pref: SandboxablePreference,
    has_managed_network_requirements: bool,
) -> bool {
    match pref {
        SandboxablePreference::Forbid => false,
        SandboxablePreference::Require => true,
        SandboxablePreference::Auto => should_require_platform_sandbox(
            file_system_policy,
            network_policy,
            has_managed_network_requirements,
        ),
    }
}

pub fn transform(
    request: SandboxTransformRequest<'_>,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    let SandboxTransformRequest {
        mut command,
        permissions,
        sandbox,
        enforce_managed_network: _,
        environment_id,
        network,
        sandbox_policy_cwd,
        windows_sandbox_level,
        windows_sandbox_private_desktop,
    } = request;
    let additional_permissions = command.additional_permissions.take();
    let managed_mitm_ca_trust_bundle_path =
        network.and_then(NetworkProxy::managed_mitm_ca_trust_bundle_path);
    let base_effective_permission_profile =
        effective_permission_profile(permissions, additional_permissions.as_ref());
    let pending_sandboxed_request = PendingSandboxedExecRequest::new(
        &command.cwd,
        sandbox_policy_cwd,
        base_effective_permission_profile.clone(),
        managed_mitm_ca_trust_bundle_path.as_ref(),
    );
    let (base_file_system_policy, base_network_policy) =
        base_effective_permission_profile.to_runtime_permissions();
    let mut argv = Vec::with_capacity(1 + command.args.len());
    argv.push(command.program);
    argv.extend(command.args.into_iter().map(OsString::from));

    let (argv, arg0_override, pending_sandboxed_request) = match sandbox {
        SandboxType::None => (os_argv_to_strings(argv), None, None),
        SandboxType::WindowsRestrictedToken => (
            os_argv_to_strings(argv),
            None,
            Some(pending_sandboxed_request?),
        ),
    };

    // Unsandboxed exec-server requests may have foreign cwd values that cannot be prepared
    // locally, but their effective permissions must still be preserved. In that case, carry
    // forward the base profile and its derived runtime policies.
    let (permission_profile, file_system_sandbox_policy, network_sandbox_policy) =
        pending_sandboxed_request.map_or(
            (
                base_effective_permission_profile,
                base_file_system_policy,
                base_network_policy,
            ),
            |pending| {
                (
                    pending.effective_permission_profile,
                    pending.effective_file_system_policy,
                    pending.effective_network_policy,
                )
            },
        );

    Ok(SandboxExecRequest {
        command: argv,
        cwd: command.cwd,
        sandbox_policy_cwd: sandbox_policy_cwd.clone(),
        env: command.env,
        network: network.cloned(),
        network_environment_id: environment_id.map(str::to_string),
        sandbox,
        windows_sandbox_level,
        windows_sandbox_private_desktop,
        permission_profile,
        file_system_sandbox_policy,
        network_sandbox_policy,
        arg0: arg0_override,
    })
}

pub fn transform_for_direct_spawn(
    request: SandboxDirectSpawnTransformRequest<'_>,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    let codex_home = codex_utils_home_dir::find_codex_home()
        .map_err(|err| SandboxTransformError::WindowsSandboxPreparation(err.to_string()))?;
    transform_for_direct_spawn_with_codex_home(request, codex_home.as_path())
}

fn transform_for_direct_spawn_with_codex_home(
    request: SandboxDirectSpawnTransformRequest<'_>,
    codex_home: &Path,
) -> Result<SandboxExecRequest, SandboxTransformError> {
    let workspace_roots = request.workspace_roots;
    let proxy_settings_mode = request.windows_sandbox_proxy_settings_mode;
    let mut request = transform(request.transform)?;
    if request.sandbox == SandboxType::WindowsRestrictedToken {
        wrap_windows_sandbox_exec_request_for_direct_spawn(
            &mut request,
            workspace_roots,
            codex_home,
            proxy_settings_mode,
        )?;
    }
    Ok(request)
}

fn wrap_windows_sandbox_exec_request_for_direct_spawn(
    request: &mut SandboxExecRequest,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    proxy_settings_mode: codex_windows_sandbox::WindowsSandboxProxySettingsMode,
) -> Result<(), SandboxTransformError> {
    // TODO(anp): Keep PathUri through the Windows sandbox wrapper boundary.
    let native_cwd =
        request
            .cwd
            .to_abs_path()
            .map_err(|source| SandboxTransformError::InvalidCommandCwd {
                cwd: request.cwd.clone(),
                source,
            })?;
    let native_sandbox_policy_cwd = request.sandbox_policy_cwd.to_abs_path().map_err(|source| {
        SandboxTransformError::InvalidSandboxPolicyCwd {
            cwd: request.sandbox_policy_cwd.clone(),
            source,
        }
    })?;
    let Some(program) = request.command.first_mut() else {
        return Err(SandboxTransformError::WindowsSandboxPreparation(
            "sandbox command was empty".to_string(),
        ));
    };
    let source = std::path::PathBuf::from(&program);
    let (helper, materialization_status) =
        codex_windows_sandbox::resolve_exe_for_launch_with_status(source.as_path(), codex_home);
    tracing::debug!(
        helper_materialization_status = ?materialization_status,
        helper_source = %source.display(),
        helper_path = %helper.display(),
        "resolved Windows sandbox helper materialization"
    );
    *program = helper.to_string_lossy().into_owned();

    let inner_command = std::mem::take(&mut request.command);
    let proxy_enforced = request.network.is_some();
    let use_elevated =
        windows_sandbox_uses_elevated_backend(request.windows_sandbox_level, proxy_enforced);
    let overrides = if use_elevated {
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
    .map_err(SandboxTransformError::WindowsSandboxPreparation)?;
    let empty_paths: &[AbsolutePathBuf] = &[];
    let read_roots_override = overrides
        .as_ref()
        .and_then(|overrides| overrides.read_roots_override.as_deref());
    let read_roots_include_platform_defaults = overrides
        .as_ref()
        .is_some_and(|overrides| overrides.read_roots_include_platform_defaults);
    let write_roots_override = overrides
        .as_ref()
        .and_then(|overrides| overrides.write_roots_override.as_deref());
    let deny_read_paths_override = overrides.as_ref().map_or(empty_paths, |overrides| {
        overrides.additional_deny_read_paths.as_slice()
    });
    let deny_write_paths_override = overrides.as_ref().map_or(empty_paths, |overrides| {
        overrides.additional_deny_write_paths.as_slice()
    });
    let mut wrapper_args =
        codex_windows_sandbox::create_windows_sandbox_command_args_for_permission_profile(
            inner_command,
            &native_cwd,
            workspace_roots,
            &request.env,
            &request.permission_profile,
            request.windows_sandbox_level,
            request.windows_sandbox_private_desktop,
            proxy_enforced,
            proxy_settings_mode,
            read_roots_override,
            read_roots_include_platform_defaults,
            write_roots_override,
            deny_read_paths_override,
            deny_write_paths_override,
            codex_home,
        );

    request.command = Vec::with_capacity(1 + wrapper_args.len());
    request.command.push(source.to_string_lossy().into_owned());
    request.command.append(&mut wrapper_args);
    request.sandbox = SandboxType::None;
    request.arg0 = None;
    add_windows_sandbox_wrapper_setup_env(&mut request.env);
    Ok(())
}

fn add_windows_sandbox_wrapper_setup_env(env: &mut HashMap<String, String>) {
    add_windows_sandbox_wrapper_setup_env_from_vars(env, std::env::vars_os());
}

fn add_windows_sandbox_wrapper_setup_env_from_vars(
    env: &mut HashMap<String, String>,
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) {
    for (key, value) in vars {
        let key = key.to_string_lossy().into_owned();
        if !WINDOWS_SANDBOX_WRAPPER_SETUP_ENV_ALLOWLIST
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
        {
            continue;
        }
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(&key));
        env.insert(key, value.to_string_lossy().into_owned());
    }
}

pub fn compatibility_sandbox_policy_for_permission_profile(
    permissions: &PermissionProfile,
    cwd: &Path,
) -> SandboxPolicy {
    permissions
        .to_legacy_sandbox_policy(cwd)
        .unwrap_or_else(|_| {
            let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
            compatibility_workspace_write_policy(file_system_policy, network_policy, cwd)
        })
}

fn compatibility_workspace_write_policy(
    file_system_policy: FileSystemSandboxPolicy,
    network_policy: NetworkSandboxPolicy,
    cwd: &Path,
) -> SandboxPolicy {
    let cwd_abs = AbsolutePathBuf::from_absolute_path(cwd).ok();
    let writable_roots = file_system_policy
        .get_writable_roots_with_cwd(cwd)
        .into_iter()
        .map(|root| root.root)
        .filter(|root| cwd_abs.as_ref() != Some(root))
        .collect();
    let tmpdir_writable = std::env::var_os("TMPDIR")
        .filter(|tmpdir| !tmpdir.is_empty())
        .and_then(|tmpdir| {
            AbsolutePathBuf::from_absolute_path(std::path::PathBuf::from(tmpdir)).ok()
        })
        .is_some_and(|tmpdir| file_system_policy.can_write_path_with_cwd(tmpdir.as_path(), cwd));
    let slash_tmp = Path::new("/tmp");
    let slash_tmp_writable = slash_tmp.is_absolute()
        && slash_tmp.is_dir()
        && file_system_policy.can_write_path_with_cwd(slash_tmp, cwd);

    SandboxPolicy::WorkspaceWrite {
        writable_roots,
        network_access: network_policy.is_enabled(),
        exclude_tmpdir_env_var: !tmpdir_writable,
        exclude_slash_tmp: !slash_tmp_writable,
    }
}

fn os_argv_to_strings(argv: Vec<OsString>) -> Vec<String> {
    argv.into_iter()
        .map(os_string_to_command_component)
        .collect()
}

fn os_string_to_command_component(value: OsString) -> String {
    value
        .into_string()
        .unwrap_or_else(|value| value.to_string_lossy().into_owned())
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
