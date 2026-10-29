use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use crate::allow::AllowDenyPaths;
use crate::allow::compute_allow_paths_for_permissions;
use crate::helper_materialization::bundled_executable_path_for_exe;
use crate::helper_materialization::helper_bin_dir;
use crate::identity::sandbox_setup_is_complete;
use crate::logging::current_log_file_path;
use crate::logging::log_note;
use crate::path_normalization::canonical_path_key;
use crate::path_normalization::canonicalize_path;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::setup_error::SetupErrorCode;
use crate::setup_error::SetupFailure;
use crate::setup_error::clear_setup_error_report;
use crate::setup_error::failure;
use crate::setup_error::read_setup_error_report;
use crate::setup_protocol::SetupMode;
use crate::setup_protocol::SetupPayload;
use crate::ssh_config_dependencies::ssh_config_dependency_paths;
use anyhow::Result;
use anyhow::anyhow;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Security::AllocateAndInitializeSid;
use windows_sys::Win32::Security::CheckTokenMembership;
use windows_sys::Win32::Security::FreeSid;
use windows_sys::Win32::Security::SECURITY_NT_AUTHORITY;

pub const SETUP_VERSION: u32 = 7;
pub const OFFLINE_USERNAME: &str = "CodexSandboxOffline";
pub const ONLINE_USERNAME: &str = "CodexSandboxOnline";
const ERROR_CANCELLED: u32 = 1223;
const SECURITY_BUILTIN_DOMAIN_RID: u32 = 0x0000_0020;
const DOMAIN_ALIAS_RID_ADMINS: u32 = 0x0000_0220;
const SETUP_EXE_FILENAME: &str = "codex-windows-sandbox-setup.exe";
const USERPROFILE_ROOT_EXCLUSIONS: &[&str] = &[
    ".ssh",
    ".tsh",
    ".brev",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config",
    ".npm",
    ".pki",
    ".terraform.d",
];
const WINDOWS_PLATFORM_DEFAULT_READ_ROOTS: &[&str] = &[
    r"C:\Windows",
    r"C:\Program Files",
    r"C:\Program Files (x86)",
    r"C:\ProgramData",
];

pub fn sandbox_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(".sandbox")
}

pub fn sandbox_bin_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(".sandbox-bin")
}

pub fn sandbox_secrets_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(".sandbox-secrets")
}

pub fn setup_marker_path(codex_home: &Path) -> PathBuf {
    sandbox_dir(codex_home).join("setup_marker.json")
}

pub fn sandbox_users_path(codex_home: &Path) -> PathBuf {
    sandbox_secrets_dir(codex_home).join("sandbox_users.json")
}

pub struct SandboxSetupRequest<'a> {
    pub permissions: &'a ResolvedWindowsSandboxPermissions,
    pub command_cwd: &'a Path,
    pub env_map: &'a HashMap<String, String>,
    pub codex_home: &'a Path,
    pub proxy_enforced: bool,
}

#[derive(Default)]
pub struct SetupRootOverrides {
    pub read_roots: Option<Vec<PathBuf>>,
    pub read_roots_include_platform_defaults: bool,
    pub write_roots: Option<Vec<PathBuf>>,
    pub deny_read_paths: Option<Vec<PathBuf>>,
    pub deny_write_paths: Option<Vec<PathBuf>>,
}

pub fn run_setup_refresh(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    proxy_enforced: bool,
) -> Result<()> {
    let Ok(permissions) =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )
    else {
        return Ok(());
    };
    run_setup_refresh_inner(
        SandboxSetupRequest {
            permissions: &permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced,
        },
        SetupRootOverrides::default(),
        /*offline_proxy_settings_override*/ None,
        SetupMode::Full,
        None,
    )
}

pub(crate) fn run_setup_refresh_with_overrides_and_proxy_settings(
    request: SandboxSetupRequest<'_>,
    overrides: SetupRootOverrides,
    offline_proxy_settings: &OfflineProxySettings,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<()> {
    run_setup_refresh_inner(
        request,
        overrides,
        Some(offline_proxy_settings),
        SetupMode::Full,
        cancellation,
    )
}

pub fn run_setup_refresh_with_extra_read_roots(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    extra_read_roots: Vec<PathBuf>,
    proxy_enforced: bool,
) -> Result<()> {
    let Ok(permissions) =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )
    else {
        return Ok(());
    };
    let mut read_roots = gather_read_roots(command_cwd, &permissions, env_map, codex_home);
    read_roots.extend(extra_read_roots);
    run_setup_refresh_inner(
        SandboxSetupRequest {
            permissions: &permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced,
        },
        SetupRootOverrides {
            read_roots: Some(read_roots),
            read_roots_include_platform_defaults: false,
            write_roots: Some(Vec::new()),
            deny_read_paths: None,
            deny_write_paths: None,
        },
        /*offline_proxy_settings_override*/ None,
        SetupMode::Full,
        None,
    )
}

pub fn run_strict_read_root_grant(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_root: PathBuf,
    proxy_enforced: bool,
) -> Result<()> {
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;
    let mut read_roots = gather_read_roots(command_cwd, &permissions, env_map, codex_home);
    read_roots.push(read_root);
    run_setup_refresh_inner(
        SandboxSetupRequest {
            permissions: &permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced,
        },
        SetupRootOverrides {
            read_roots: Some(read_roots),
            read_roots_include_platform_defaults: false,
            write_roots: Some(Vec::new()),
            deny_read_paths: None,
            deny_write_paths: None,
        },
        /*offline_proxy_settings_override*/ None,
        SetupMode::ReadAclsOnlyStrict,
        None,
    )
}

fn run_setup_refresh_inner(
    request: SandboxSetupRequest<'_>,
    overrides: SetupRootOverrides,
    offline_proxy_settings_override: Option<&OfflineProxySettings>,
    mode: SetupMode,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<()> {
    check_setup_cancellation(cancellation)?;
    if !request.permissions.is_enforceable_by_windows_sandbox() {
        anyhow::bail!("unsupported filesystem permissions for Windows sandbox setup");
    }
    let (read_roots, write_roots) = build_payload_roots(&request, &overrides);
    let deny_read_paths = build_payload_deny_read_paths(overrides.deny_read_paths);
    let deny_write_paths = build_payload_deny_write_paths(&request, overrides.deny_write_paths);
    let offline_proxy_settings =
        offline_proxy_settings_for_request(&request, offline_proxy_settings_override);
    let payload = SetupPayload {
        version: SETUP_VERSION,
        offline_username: OFFLINE_USERNAME.to_string(),
        online_username: ONLINE_USERNAME.to_string(),
        codex_home: request.codex_home.to_path_buf(),
        command_cwd: request.command_cwd.to_path_buf(),
        read_roots,
        write_roots,
        deny_read_paths,
        deny_write_paths,
        proxy_ports: offline_proxy_settings.proxy_ports,
        allow_local_binding: offline_proxy_settings.allow_local_binding,
        otel: None,
        real_user: std::env::var("USERNAME").unwrap_or_else(|_| "Administrators".to_string()),
        mode,
        refresh_only: true,
    };
    let staged_payload = crate::setup_protocol::PreparedSetupRequest::new(&payload)?;
    let payload_args = staged_payload.arguments(false)?;
    let exe = find_setup_exe()?;
    let sbx_dir = sandbox_dir(request.codex_home);
    let log_path = current_log_file_path(&sbx_dir);
    let cleared_report = match clear_setup_error_report(request.codex_home) {
        Ok(()) => true,
        Err(err) => {
            log_note(
                &format!("setup refresh: failed to clear setup_error.json before launch: {err}"),
                Some(&sbx_dir),
            );
            false
        }
    };
    // Refresh should never request elevation; ensure verb isn't set and we don't trigger UAC.
    let mut cmd = Command::new(&exe);
    cmd.args(&payload_args).stdout(Stdio::null()).stderr(Stdio::null());
    let cwd = std::env::current_dir().unwrap_or_else(|_| request.codex_home.to_path_buf());
    log_note(
        &format!(
            "setup refresh: spawning {} (cwd={}, payload_len={})",
            exe.display(),
            cwd.display(),
            staged_payload.payload_len
        ),
        Some(&sbx_dir),
    );
    let status = wait_setup_child(&mut cmd, cancellation).map_err(|err| {
        let message = format!(
            "setup refresh failed to launch helper: helper={}, cwd={}, log={}, error={err}",
            exe.display(),
            cwd.display(),
            log_path.display()
        );
        log_note(&format!("setup refresh: {message}"), Some(&sbx_dir));
        failure(SetupErrorCode::OrchestratorHelperLaunchFailed, message)
    })?;
    if !status.success() {
        log_note(
            &format!("setup refresh: exited with status {status:?}"),
            Some(&sbx_dir),
        );
        return Err(report_helper_failure(
            request.codex_home,
            cleared_report,
            status.code(),
        ));
    }
    if let Err(err) = clear_setup_error_report(request.codex_home) {
        log_note(
            &format!("setup refresh: failed to clear setup_error.json after success: {err}"),
            Some(&sbx_dir),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupMarker {
    pub version: u32,
    pub offline_username: String,
    pub online_username: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub proxy_ports: Vec<u16>,
    #[serde(default)]
    pub allow_local_binding: bool,
}

impl SetupMarker {
    pub fn version_matches(&self) -> bool {
        self.version == SETUP_VERSION
    }

    pub(crate) fn offline_proxy_settings(&self) -> OfflineProxySettings {
        OfflineProxySettings {
            proxy_ports: self.proxy_ports.clone(),
            allow_local_binding: self.allow_local_binding,
        }
    }

    pub(crate) fn request_mismatch_reason(
        &self,
        network_identity: SandboxNetworkIdentity,
        offline_proxy_settings: &OfflineProxySettings,
    ) -> Option<String> {
        if !network_identity.uses_offline_identity() {
            return None;
        }
        if self.proxy_ports == offline_proxy_settings.proxy_ports
            && self.allow_local_binding == offline_proxy_settings.allow_local_binding
        {
            return None;
        }
        Some(format!(
            "offline firewall settings changed (stored_ports={:?}, desired_ports={:?}, stored_allow_local_binding={}, desired_allow_local_binding={})",
            self.proxy_ports,
            offline_proxy_settings.proxy_ports,
            self.allow_local_binding,
            offline_proxy_settings.allow_local_binding
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxUserRecord {
    pub username: String,
    /// DPAPI-encrypted password blob, base64 encoded.
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxUsersFile {
    pub version: u32,
    pub offline: SandboxUserRecord,
    pub online: SandboxUserRecord,
}

impl SandboxUsersFile {
    pub fn version_matches(&self) -> bool {
        self.version == SETUP_VERSION
    }
}

fn is_elevated() -> Result<bool> {
    // SAFETY: AllocateAndInitializeSid receives a valid authority and writable SID output; the SID
    // stays allocated through CheckTokenMembership and is released with FreeSid afterward.
    unsafe {
        let mut administrators_group: *mut c_void = std::ptr::null_mut();
        let ok = AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID,
            DOMAIN_ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut administrators_group,
        );
        if ok == 0 {
            return Err(anyhow!(
                "AllocateAndInitializeSid failed: {}",
                GetLastError()
            ));
        }
        let mut is_member = 0i32;
        let check = CheckTokenMembership(
            std::ptr::null_mut(),
            administrators_group,
            &mut is_member as *mut _,
        );
        FreeSid(administrators_group as *mut _);
        if check == 0 {
            return Err(anyhow!("CheckTokenMembership failed: {}", GetLastError()));
        }
        Ok(is_member != 0)
    }
}

fn canonical_existing(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|p| {
            if !p.exists() {
                return None;
            }
            Some(dunce::canonicalize(p).unwrap_or_else(|_| p.clone()))
        })
        .collect()
}

fn profile_read_roots(user_profile: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(user_profile) {
        Ok(entries) => entries,
        Err(_) => return vec![user_profile.to_path_buf()],
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| (entry.file_name(), entry.path()))
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !USERPROFILE_ROOT_EXCLUSIONS
                .iter()
                .any(|excluded| name.eq_ignore_ascii_case(excluded))
        })
        .map(|(_, path)| path)
        .collect()
}

fn gather_helper_read_roots(codex_home: &Path) -> Vec<PathBuf> {
    let helper_dir = helper_bin_dir(codex_home);
    let _ = std::fs::create_dir_all(&helper_dir);
    vec![helper_dir]
}

fn gather_full_read_roots_for_permissions(
    command_cwd: &Path,
    permissions: &ResolvedWindowsSandboxPermissions,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> Vec<PathBuf> {
    let mut roots = gather_helper_read_roots(codex_home);
    roots.extend(
        WINDOWS_PLATFORM_DEFAULT_READ_ROOTS
            .iter()
            .map(PathBuf::from),
    );
    if let Ok(up) = std::env::var("USERPROFILE") {
        roots.extend(profile_read_roots(Path::new(&up)));
    }
    roots.push(command_cwd.to_path_buf());
    roots.extend(
        permissions
            .writable_roots_for_cwd(command_cwd, env_map)
            .into_iter()
            .map(|root| root.root),
    );
    canonical_existing(&roots)
}

pub(crate) fn gather_read_roots(
    command_cwd: &Path,
    permissions: &ResolvedWindowsSandboxPermissions,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> Vec<PathBuf> {
    if permissions.has_full_disk_read_access() {
        return gather_full_read_roots_for_permissions(
            command_cwd,
            permissions,
            env_map,
            codex_home,
        );
    }

    let mut roots = gather_helper_read_roots(codex_home);
    if permissions.include_platform_defaults() {
        roots.extend(
            WINDOWS_PLATFORM_DEFAULT_READ_ROOTS
                .iter()
                .map(PathBuf::from),
        );
    }
    roots.extend(permissions.readable_roots_for_cwd(command_cwd));
    canonical_existing(&roots)
}

pub(crate) fn gather_write_roots_for_permissions(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
) -> Vec<PathBuf> {
    let roots = permissions
        .writable_roots_for_cwd(command_cwd, env_map)
        .into_iter()
        .map(|root| root.root)
        .collect::<Vec<_>>();
    let mut dedup: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in canonical_existing(&roots) {
        if dedup.insert(r.clone()) {
            out.push(r);
        }
    }
    out
}

pub(crate) fn effective_write_roots_for_setup(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    write_roots_override: Option<&[PathBuf]>,
) -> Vec<PathBuf> {
    effective_write_roots_for_permissions(
        permissions,
        command_cwd,
        env_map,
        codex_home,
        write_roots_override,
    )
}

pub(crate) fn effective_write_roots_for_permissions(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    write_roots_override: Option<&[PathBuf]>,
) -> Vec<PathBuf> {
    let write_roots = if let Some(roots) = write_roots_override {
        canonical_existing(roots)
    } else {
        gather_write_roots_for_permissions(permissions, command_cwd, env_map)
    };
    let write_roots = filter_user_profile_roots(write_roots);
    filter_sensitive_write_roots(write_roots, codex_home)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfflineProxySettings {
    pub proxy_ports: Vec<u16>,
    pub allow_local_binding: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxNetworkIdentity {
    Offline,
    Online,
}

impl SandboxNetworkIdentity {
    pub(crate) fn from_permissions(
        permissions: &ResolvedWindowsSandboxPermissions,
        proxy_enforced: bool,
    ) -> Self {
        if proxy_enforced || !permissions.network_policy().is_enabled() {
            Self::Offline
        } else {
            Self::Online
        }
    }

    pub(crate) fn uses_offline_identity(self) -> bool {
        matches!(self, Self::Offline)
    }
}

const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ws_proxy",
    "wss_proxy",
];
const ALLOW_LOCAL_BINDING_ENV_KEY: &str = "CODEX_NETWORK_ALLOW_LOCAL_BINDING";

pub(crate) fn offline_proxy_settings_from_env(
    env_map: &HashMap<String, String>,
    network_identity: SandboxNetworkIdentity,
) -> OfflineProxySettings {
    if !network_identity.uses_offline_identity() {
        return OfflineProxySettings {
            proxy_ports: vec![],
            allow_local_binding: false,
        };
    }
    OfflineProxySettings {
        proxy_ports: proxy_ports_from_env(env_map),
        allow_local_binding: env_map
            .get(ALLOW_LOCAL_BINDING_ENV_KEY)
            .is_some_and(|value| value == "1"),
    }
}

fn offline_proxy_settings_for_request(
    request: &SandboxSetupRequest<'_>,
    offline_proxy_settings_override: Option<&OfflineProxySettings>,
) -> OfflineProxySettings {
    offline_proxy_settings_override.cloned().unwrap_or_else(|| {
        let network_identity =
            SandboxNetworkIdentity::from_permissions(request.permissions, request.proxy_enforced);
        offline_proxy_settings_from_env(request.env_map, network_identity)
    })
}

pub(crate) fn proxy_ports_from_env(env_map: &HashMap<String, String>) -> Vec<u16> {
    let mut ports = BTreeSet::new();
    for key in PROXY_ENV_KEYS {
        if let Some(value) = env_map.get(*key)
            && let Some(port) = loopback_proxy_port_from_url(value)
        {
            ports.insert(port);
        }
    }
    ports.into_iter().collect()
}

fn loopback_proxy_port_from_url(url: &str) -> Option<u16> {
    let authority = url.trim().split_once("://")?.1.split('/').next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);

    if let Some(host) = host_port.strip_prefix('[') {
        let (host, rest) = host.split_once(']')?;
        if host != "::1" {
            return None;
        }
        let port = rest.strip_prefix(':')?.parse::<u16>().ok()?;
        return (port != 0).then_some(port);
    }

    let (host, port) = host_port.rsplit_once(':')?;
    if !(host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1") {
        return None;
    }
    let port = port.parse::<u16>().ok()?;
    (port != 0).then_some(port)
}

fn quote_arg(arg: &str) -> String {
    let needs = arg.is_empty()
        || arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"'));
    if !needs {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut bs = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                bs += 1;
            }
            '"' => {
                out.push_str(&"\\".repeat(bs * 2 + 1));
                out.push('"');
                bs = 0;
            }
            _ => {
                if bs > 0 {
                    out.push_str(&"\\".repeat(bs));
                    bs = 0;
                }
                out.push(ch);
            }
        }
    }
    if bs > 0 {
        out.push_str(&"\\".repeat(bs * 2));
    }
    out.push('"');
    out
}

fn find_setup_exe() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(setup_exe) = find_setup_exe_for_current_exe(&exe)
        && setup_exe.is_absolute()
        && setup_exe.is_file()
    {
        return Ok(setup_exe);
    }
    Err(failure(SetupErrorCode::OrchestratorHelperLaunchFailed,
        "packaged Windows sandbox setup helper was not found; refusing ambient executable lookup"))
}

fn find_setup_exe_for_current_exe(exe: &Path) -> Option<PathBuf> {
    bundled_executable_path_for_exe(exe, SETUP_EXE_FILENAME)
}

fn report_helper_failure(
    codex_home: &Path,
    cleared_report: bool,
    exit_code: Option<i32>,
) -> anyhow::Error {
    let exit_detail = format!("setup helper exited with status {exit_code:?}");
    if !cleared_report {
        return failure(SetupErrorCode::OrchestratorHelperExitNonzero, exit_detail);
    }
    match read_setup_error_report(codex_home) {
        Ok(Some(report)) => anyhow::Error::new(SetupFailure::from_report(report)),
        Ok(None) => failure(SetupErrorCode::OrchestratorHelperExitNonzero, exit_detail),
        Err(err) => failure(
            SetupErrorCode::OrchestratorHelperReportReadFailed,
            format!("{exit_detail}; failed to read setup_error.json: {err}"),
        ),
    }
}

fn verify_setup_completed(codex_home: &Path) -> Result<()> {
    if sandbox_setup_is_complete(codex_home) {
        Ok(())
    } else {
        Err(failure(
            SetupErrorCode::OrchestratorHelperIncomplete,
            "setup helper exited successfully before setup completed",
        ))
    }
}

pub(crate) fn check_setup_cancellation(cancellation: Option<&crate::WindowsSandboxCancellationToken>) -> Result<()> {
    anyhow::ensure!(!cancellation.is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled), "sandbox setup cancelled");
    Ok(())
}

fn wait_setup_child(command: &mut Command, cancellation: Option<&crate::WindowsSandboxCancellationToken>) -> Result<std::process::ExitStatus> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    check_setup_cancellation(cancellation)?;
    let mut child = command.spawn()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(err) => { let _ = child.kill(); let _ = child.wait(); return Err(err.into()); }
        }
        if let Err(err) = check_setup_cancellation(cancellation) {
            let killed = child.kill();
            let reaped = child.wait();
            return Err(err.context(format!("setup helper termination: {killed:?}; reap: {reaped:?}")));
        }
        // Completion wakes this wait immediately; the timeout only bounds how
        // long cancellation can go unchecked. The child retains handle ownership.
        // SAFETY: child owns this process handle and cannot close it during this
        // synchronous, bounded wait. Waiting does not consume the handle.
        let waited = unsafe { WaitForSingleObject(child.as_raw_handle().cast(), 25) };
        let error = match waited {
            WAIT_OBJECT_0 | WAIT_TIMEOUT => continue,
            WAIT_FAILED => std::io::Error::last_os_error(),
            result => std::io::Error::other(format!(
                "unexpected setup helper wait result: {result}"
            )),
        };
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
}

fn run_setup_exe(payload: &SetupPayload, needs_elevation: bool, codex_home: &Path, cancellation: Option<&crate::WindowsSandboxCancellationToken>) -> Result<()> {
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    use windows_sys::Win32::UI::Shell::SEE_MASK_NOCLOSEPROCESS;
    use windows_sys::Win32::UI::Shell::SHELLEXECUTEINFOW;
    use windows_sys::Win32::UI::Shell::ShellExecuteExW;
    check_setup_cancellation(cancellation)?;
    let exe = find_setup_exe()?;
    if std::env::var("CODEX_WINDOWS_SANDBOX_DENY_PROVISIONING").as_deref() == Ok("1")
        && matches!(&payload.mode, SetupMode::Full | SetupMode::ProvisionOnly)
    {
        anyhow::bail!(
            "Windows sandbox provisioning is disabled for this process; reuse an already provisioned home or explicitly authorize setup"
        );
    }
    let staged_payload = crate::setup_protocol::PreparedSetupRequest::new(payload).map_err(|err| {
        failure(
            SetupErrorCode::OrchestratorPayloadSerializeFailed,
            format!("failed to serialize elevation payload: {err}"),
        )
    })?;
    let payload_args = staged_payload.arguments(false)?;
    let cleared_report = match clear_setup_error_report(codex_home) {
        Ok(()) => true,
        Err(err) => {
            log_note(
                &format!(
                    "setup orchestrator: failed to clear setup_error.json before launch: {err}"
                ),
                Some(&sandbox_dir(codex_home)),
            );
            false
        }
    };

    if !needs_elevation {
        let mut command = Command::new(&exe);
        command
            .args(&payload_args)
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            ;
        let status = wait_setup_child(&mut command, cancellation)
            .map_err(|err| {
                failure(
                    SetupErrorCode::OrchestratorHelperLaunchFailed,
                    format!("failed to launch setup helper (non-elevated): {err}"),
                )
            })?;
        if !status.success() {
            return Err(report_helper_failure(
                codex_home,
                cleared_report,
                status.code(),
            ));
        }
        verify_setup_completed(codex_home)?;
        if let Err(err) = clear_setup_error_report(codex_home) {
            log_note(
                &format!(
                    "setup orchestrator: failed to clear setup_error.json after success: {err}"
                ),
                Some(&sandbox_dir(codex_home)),
            );
        }
        return Ok(());
    }

    let exe_w = crate::winutil::to_wide(&exe);
    let params = payload_args.iter().map(|arg| quote_arg(arg)).collect::<Vec<_>>().join(" ");
    let params_w = crate::winutil::to_wide(params);
    let verb_w = crate::winutil::to_wide("runas");
    // SAFETY: SHELLEXECUTEINFOW contains integer fields and nullable pointers, so zero is a valid
    // initial representation before required fields are assigned.
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.lpVerb = verb_w.as_ptr();
    sei.lpFile = exe_w.as_ptr();
    sei.lpParameters = params_w.as_ptr();
    // Hide the window for the elevated helper.
    sei.nShow = 0; // SW_HIDE
    // SAFETY: sei has its required size and points to live NUL-terminated verb, executable and
    // parameter buffers; the process handle output remains in sei for checked ownership
    // below.
    check_setup_cancellation(cancellation)?;
    // ShellExecuteExW's UAC consent UI itself has no cancellation handle.
    let ok = unsafe { ShellExecuteExW(&mut sei) };
    if ok == 0 || sei.hProcess.is_null() {
        // SAFETY: Read the thread-local ShellExecuteExW status before any further native call can
        // replace it; GetLastError takes no pointers.
        let last_error = unsafe { GetLastError() };
        let code = if last_error == ERROR_CANCELLED {
            SetupErrorCode::OrchestratorHelperLaunchCanceled
        } else {
            SetupErrorCode::OrchestratorHelperLaunchFailed
        };
        return Err(failure(
            code,
            format!("ShellExecuteExW failed to launch setup helper: {last_error}"),
        ));
    }
    // SAFETY: sei.hProcess is the checked successful handle retained by this function; the exit-
    // code slot is writable, and the handle is closed once after the wait and query.
    unsafe {
        loop {
            let waited = WaitForSingleObject(sei.hProcess, 50);
            if waited == 0 { break; }
            if waited != 258 {
                let error = std::io::Error::last_os_error();
                let _ = windows_sys::Win32::System::Threading::TerminateProcess(sei.hProcess, 1);
                CloseHandle(sei.hProcess);
                return Err(error.into());
            }
            if check_setup_cancellation(cancellation).is_err() {
                let terminated = windows_sys::Win32::System::Threading::TerminateProcess(sei.hProcess, 1);
                let reaped = WaitForSingleObject(sei.hProcess, 5000) == 0;
                CloseHandle(sei.hProcess);
                anyhow::bail!("sandbox setup cancelled (termination accepted: {}, reaped: {})", terminated != 0, reaped);
            }
        }
        let mut code: u32 = 1;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        if code != 0 {
            return Err(report_helper_failure(
                codex_home,
                cleared_report,
                Some(code as i32),
            ));
        }
    }
    verify_setup_completed(codex_home)?;
    if let Err(err) = clear_setup_error_report(codex_home) {
        log_note(
            &format!("setup orchestrator: failed to clear setup_error.json after success: {err}"),
            Some(&sandbox_dir(codex_home)),
        );
    }
    Ok(())
}

pub fn run_elevated_setup(
    request: SandboxSetupRequest<'_>,
    overrides: SetupRootOverrides,
) -> Result<()> {
    run_elevated_setup_inner(
        request, overrides, /*offline_proxy_settings_override*/ None, None,
    )
}

pub(crate) fn run_elevated_setup_with_proxy_settings(
    request: SandboxSetupRequest<'_>,
    overrides: SetupRootOverrides,
    offline_proxy_settings: &OfflineProxySettings,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<()> {
    run_elevated_setup_inner(request, overrides, Some(offline_proxy_settings), cancellation)
}

fn run_elevated_setup_inner(
    request: SandboxSetupRequest<'_>,
    overrides: SetupRootOverrides,
    offline_proxy_settings_override: Option<&OfflineProxySettings>,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<()> {
    check_setup_cancellation(cancellation)?;
    if !request.permissions.is_enforceable_by_windows_sandbox() {
        anyhow::bail!("unsupported filesystem permissions for Windows sandbox setup");
    }
    // Ensure the shared sandbox directory exists before we send it to the elevated helper.
    let sbx_dir = sandbox_dir(request.codex_home);
    std::fs::create_dir_all(&sbx_dir).map_err(|err| {
        failure(
            SetupErrorCode::OrchestratorSandboxDirCreateFailed,
            format!("failed to create sandbox dir {}: {err}", sbx_dir.display()),
        )
    })?;
    let (read_roots, write_roots) = build_payload_roots(&request, &overrides);
    let deny_read_paths = build_payload_deny_read_paths(overrides.deny_read_paths);
    let deny_write_paths = build_payload_deny_write_paths(&request, overrides.deny_write_paths);
    let offline_proxy_settings =
        offline_proxy_settings_for_request(&request, offline_proxy_settings_override);
    let payload = SetupPayload {
        version: SETUP_VERSION,
        offline_username: OFFLINE_USERNAME.to_string(),
        online_username: ONLINE_USERNAME.to_string(),
        codex_home: request.codex_home.to_path_buf(),
        command_cwd: request.command_cwd.to_path_buf(),
        read_roots,
        write_roots,
        deny_read_paths,
        deny_write_paths,
        proxy_ports: offline_proxy_settings.proxy_ports,
        allow_local_binding: offline_proxy_settings.allow_local_binding,
        real_user: std::env::var("USERNAME").unwrap_or_else(|_| "Administrators".to_string()),
        otel: codex_otel::global_statsig_metrics_settings(),
        mode: SetupMode::Full,
        refresh_only: false,
    };
    let needs_elevation = !is_elevated().map_err(|err| {
        failure(
            SetupErrorCode::OrchestratorElevationCheckFailed,
            format!("failed to determine elevation state: {err}"),
        )
    })?;
    run_setup_exe(&payload, needs_elevation, request.codex_home, cancellation)
}

pub fn run_elevated_provisioning_setup(codex_home: &Path, real_user: &str) -> Result<()> {
    let sbx_dir = sandbox_dir(codex_home);
    std::fs::create_dir_all(&sbx_dir).map_err(|err| {
        failure(
            SetupErrorCode::OrchestratorSandboxDirCreateFailed,
            format!("failed to create sandbox dir {}: {err}", sbx_dir.display()),
        )
    })?;
    if !is_elevated().map_err(|err| {
        failure(
            SetupErrorCode::OrchestratorElevationCheckFailed,
            format!("failed to determine elevation state: {err}"),
        )
    })? {
        return Err(failure(
            SetupErrorCode::OrchestratorElevationRequired,
            "sandbox provisioning setup must be run from an elevated process",
        ));
    }
    let payload = SetupPayload {
        version: SETUP_VERSION,
        offline_username: OFFLINE_USERNAME.to_string(),
        online_username: ONLINE_USERNAME.to_string(),
        codex_home: codex_home.to_path_buf(),
        command_cwd: codex_home.to_path_buf(),
        read_roots: Vec::new(),
        write_roots: Vec::new(),
        deny_read_paths: Vec::new(),
        deny_write_paths: Vec::new(),
        proxy_ports: Vec::new(),
        allow_local_binding: false,
        otel: codex_otel::global_statsig_metrics_settings(),
        real_user: real_user.to_string(),
        mode: SetupMode::ProvisionOnly,
        refresh_only: false,
    };
    run_setup_exe(&payload, /*needs_elevation*/ false, codex_home, None)
}

fn build_payload_roots(
    request: &SandboxSetupRequest<'_>,
    overrides: &SetupRootOverrides,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let write_roots = effective_write_roots_for_setup(
        request.permissions,
        request.command_cwd,
        request.env_map,
        request.codex_home,
        overrides.write_roots.as_deref(),
    );
    let mut read_roots = if let Some(roots) = overrides.read_roots.as_deref() {
        // An explicit override is the split policy's complete readable set. Keep only the
        // helper/platform roots the elevated setup needs; do not re-add legacy cwd/full-read roots.
        let mut read_roots = gather_helper_read_roots(request.codex_home);
        if overrides.read_roots_include_platform_defaults {
            read_roots.extend(
                WINDOWS_PLATFORM_DEFAULT_READ_ROOTS
                    .iter()
                    .map(PathBuf::from),
            );
        }
        read_roots.extend(roots.iter().cloned());
        canonical_existing(&read_roots)
    } else {
        gather_read_roots(
            request.command_cwd,
            request.permissions,
            request.env_map,
            request.codex_home,
        )
    };
    read_roots = filter_user_profile_roots(read_roots);
    let write_root_set: HashSet<&PathBuf> = write_roots.iter().collect();
    read_roots.retain(|root| !write_root_set.contains(&root));
    (read_roots, write_roots)
}

fn build_payload_deny_write_paths(
    request: &SandboxSetupRequest<'_>,
    explicit_deny_write_paths: Option<Vec<PathBuf>>,
) -> Vec<PathBuf> {
    let allow_deny_paths: AllowDenyPaths = compute_allow_paths_for_permissions(
        request.permissions,
        request.command_cwd,
        request.env_map,
    );
    let mut deny_write_paths: Vec<PathBuf> = explicit_deny_write_paths
        .unwrap_or_default()
        .into_iter()
        .map(|path| canonicalize_path(&path))
        .collect();
    deny_write_paths.extend(allow_deny_paths.deny);
    deny_write_paths
}

fn build_payload_deny_read_paths(explicit_deny_read_paths: Option<Vec<PathBuf>>) -> Vec<PathBuf> {
    // Keep the configured spelling here so the ACL layer can plan both the
    // lexical path and any existing canonical target for reparse-point aliases.
    explicit_deny_read_paths.unwrap_or_default()
}



fn filter_user_profile_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let Ok(user_profile) = std::env::var("USERPROFILE") else {
        return roots;
    };
    filter_user_profile_roots_for(roots, Path::new(&user_profile))
}

/// Replaces the profile root with its children, then drops profile children sandbox users must
/// not receive: configured credential directories and children holding SSH configuration
/// dependencies. Returns roots sorted and deduplicated by canonical key.
///
/// This runs for every elevated command, so each path is resolved to its canonical key once.
fn filter_user_profile_roots_for(roots: Vec<PathBuf>, user_profile: &Path) -> Vec<PathBuf> {
    let profile_key = canonical_path_key(user_profile);
    let profile_prefix = format!("{}/", profile_key.trim_end_matches('/'));
    let mut keyed_roots = Vec::with_capacity(roots.len());
    for root in roots {
        let key = canonical_path_key(&root);
        if key == profile_key {
            keyed_roots.extend(
                profile_read_roots(user_profile)
                    .into_iter()
                    .map(|child| (canonical_path_key(&child), child)),
            );
        } else {
            keyed_roots.push((key, root));
        }
    }
    keyed_roots.sort_by(|left, right| left.0.cmp(&right.0));
    keyed_roots.dedup_by(|left, right| left.0 == right.0);

    let ssh_dependency_children = match ssh_config_dependency_paths(user_profile) {
        Ok(dependency_paths) => Some(
            dependency_paths
                .iter()
                .filter_map(|path| {
                    profile_child_name(&canonical_path_key(path), &profile_prefix)
                        .map(str::to_string)
                })
                .collect::<Vec<_>>(),
        ),
        Err(err) => {
            log_note(
                &format!("SSH dependency discovery failed; excluding user profile roots: {err}"),
                None,
            );
            None
        }
    };
    keyed_roots
        .into_iter()
        .filter(|(key, _)| {
            if *key == profile_key {
                return false;
            }
            if profile_key.starts_with(&format!("{}/", key.trim_end_matches('/'))) {
                return false;
            }
            let Some(child_name) = profile_child_name(key, &profile_prefix) else {
                return true;
            };
            // An incomplete dependency list cannot safely identify which profile children
            // hold SSH credentials. Keep unrelated roots, but grant no profile children.
            let Some(ssh_dependency_children) = &ssh_dependency_children else {
                return false;
            };
            let configured_exclusion = USERPROFILE_ROOT_EXCLUSIONS
                .iter()
                .any(|excluded| child_name.eq_ignore_ascii_case(excluded));
            let ssh_dependency = ssh_dependency_children
                .iter()
                .any(|dependency| child_name.eq_ignore_ascii_case(dependency));
            !configured_exclusion && !ssh_dependency
        })
        .map(|(_, root)| root)
        .collect()
}

/// Returns the first path component of `key` below the profile, if `key` is inside it.
fn profile_child_name<'a>(key: &'a str, profile_prefix: &str) -> Option<&'a str> {
    key.strip_prefix(profile_prefix)?
        .split('/')
        .next()
        .filter(|name| !name.is_empty())
}

fn filter_sensitive_write_roots(mut roots: Vec<PathBuf>, codex_home: &Path) -> Vec<PathBuf> {
    // Never grant capability write access to CODEX_HOME or anything under CODEX_HOME/.sandbox,
    // CODEX_HOME/.sandbox-bin, or CODEX_HOME/.sandbox-secrets. These locations contain sandbox
    // control/state and helper binaries and must remain tamper-resistant.
    let codex_home_key = canonical_path_key(codex_home);
    let sbx_dir_key = canonical_path_key(&sandbox_dir(codex_home));
    let sbx_dir_prefix = format!("{}/", sbx_dir_key.trim_end_matches('/'));
    let sbx_bin_dir_key = canonical_path_key(&sandbox_bin_dir(codex_home));
    let sbx_bin_dir_prefix = format!("{}/", sbx_bin_dir_key.trim_end_matches('/'));
    let secrets_dir_key = canonical_path_key(&sandbox_secrets_dir(codex_home));
    let secrets_dir_prefix = format!("{}/", secrets_dir_key.trim_end_matches('/'));

    roots.retain(|root| {
        let key = canonical_path_key(root);
        key != codex_home_key
            && !codex_home_key.starts_with(&format!("{}/", key.trim_end_matches('/')))
            && key != sbx_dir_key
            && !key.starts_with(&sbx_dir_prefix)
            && key != sbx_bin_dir_key
            && !key.starts_with(&sbx_bin_dir_prefix)
            && key != secrets_dir_key
            && !key.starts_with(&secrets_dir_prefix)
    });
    roots
}

pub(crate) fn ensure_safe_write_roots<'a>(roots: impl IntoIterator<Item = &'a PathBuf>, codex_home: &Path) -> Result<()> {
    let home = canonical_path_key(codex_home);
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from).map(|path| canonical_path_key(&path));
    for root in roots {
        let key = canonical_path_key(root);
        let prefix = format!("{}/", key.trim_end_matches('/'));
        anyhow::ensure!(key != home && !home.starts_with(&prefix)
            && !profile.as_ref().is_some_and(|profile| profile != &key && profile.starts_with(&prefix)),
            "write root {} encloses protected sandbox/profile state; use narrower writable roots or move CODEX_HOME outside the workspace", root.display());
    }
    Ok(())
}

