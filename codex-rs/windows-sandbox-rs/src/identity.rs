use crate::cap::refresh_cap_sids_cache_from_disk;
use crate::dpapi;
use crate::logging::debug_log;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::setup::SandboxNetworkIdentity;
use crate::setup::SandboxSetupRequest;
use crate::setup::SandboxUserRecord;
use crate::setup::SandboxUsersFile;
use crate::setup::SetupMarker;
use crate::setup::SetupRootOverrides;
use crate::setup::gather_read_roots;
use crate::setup::gather_write_roots_for_permissions;
use crate::setup::offline_proxy_settings_from_env;
use crate::setup::resolve_sandbox_setup_paths;
use crate::setup::run_elevated_setup_with_proxy_settings;
use crate::setup::run_setup_refresh_with_overrides_and_proxy_settings;
use crate::setup::sandbox_users_path;
use crate::setup::setup_marker_path;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Debug, Clone)]
struct SandboxIdentity {
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
pub struct SandboxCreds {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalWindowsSandboxLaunchSpec {
    permissions: ResolvedWindowsSandboxPermissions,
    command_cwd: String,
    codex_home: String,
    read_roots: Vec<String>,
    write_roots: Vec<String>,
    deny_read_paths: Vec<String>,
    deny_write_paths: Vec<String>,
    read_roots_include_platform_defaults: bool,
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    network_identity: SandboxNetworkIdentity,
    offline_proxy_settings: crate::setup::OfflineProxySettings,
}

struct PreparedCanonicalWindowsSandboxLaunchState {
    attempt_id: String,
    launch_identity: String,
    spec: CanonicalWindowsSandboxLaunchSpec,
    credentials: SandboxCreds,
}

/// One-shot credentials prepared for one exact canonical certification launch.
///
/// The credentials and resolved permission specification are intentionally
/// opaque and have no serialization representation. Clones share one consumed
/// state, so only one elevated launch can claim the prepared credentials.
#[derive(Clone)]
pub struct PreparedCanonicalWindowsSandboxLaunch {
    state: Arc<Mutex<Option<PreparedCanonicalWindowsSandboxLaunchState>>>,
}

impl fmt::Debug for PreparedCanonicalWindowsSandboxLaunch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedCanonicalWindowsSandboxLaunch")
            .field("state", &"opaque")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct AcquiredSandboxCreds {
    pub(crate) credentials: SandboxCreds,
    pub(crate) used_prepared_launch: bool,
}

/// Returns true when the on-disk setup artifacts exist and match the current
/// setup version.
///
/// This is a coarse readiness check; `require_logon_sandbox_creds` performs the
/// additional runtime validation for offline firewall settings.
pub fn sandbox_setup_is_complete(codex_home: &Path) -> bool {
    let marker_ok = matches!(load_marker(codex_home), Ok(Some(marker)) if marker.version_matches());
    if !marker_ok {
        return false;
    }
    matches!(load_users(codex_home), Ok(Some(users)) if users.version_matches())
}

fn load_marker(codex_home: &Path) -> Result<Option<SetupMarker>> {
    let path = setup_marker_path(codex_home);
    let marker = match fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str::<SetupMarker>(&contents) {
            Ok(m) => Some(m),
            Err(err) => {
                debug_log(
                    &format!("sandbox setup marker parse failed: {err}"),
                    Some(codex_home),
                );
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            debug_log(
                &format!("sandbox setup marker read failed: {err}"),
                Some(codex_home),
            );
            None
        }
    };
    Ok(marker)
}

fn load_users(codex_home: &Path) -> Result<Option<SandboxUsersFile>> {
    let path = sandbox_users_path(codex_home);
    let file = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            debug_log(
                &format!("sandbox users read failed: {err}"),
                Some(codex_home),
            );
            return Ok(None);
        }
    };
    match serde_json::from_str::<SandboxUsersFile>(&file) {
        Ok(users) => Ok(Some(users)),
        Err(err) => {
            debug_log(
                &format!("sandbox users parse failed: {err}"),
                Some(codex_home),
            );
            Ok(None)
        }
    }
}

fn remove_sandbox_users_file(codex_home: &Path, reason: &str) -> Result<()> {
    let path = sandbox_users_path(codex_home);
    debug_log(
        &format!("{reason}; deleting {}", path.display()),
        Some(codex_home),
    );
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("delete {}", path.display())),
    }
}

fn decode_password(record: &SandboxUserRecord) -> Result<String> {
    let blob = BASE64_STANDARD
        .decode(record.password.as_bytes())
        .context("base64 decode password")?;
    let decrypted = dpapi::unprotect(&blob)?;
    let pwd = String::from_utf8(decrypted).context("sandbox password not utf-8")?;
    Ok(pwd)
}

fn select_identity(
    network_identity: SandboxNetworkIdentity,
    codex_home: &Path,
) -> Result<Option<SandboxIdentity>> {
    let _marker = match load_marker(codex_home)? {
        Some(m) if m.version_matches() => m,
        _ => return Ok(None),
    };
    let users = match load_users(codex_home)? {
        Some(u) if u.version_matches() => u,
        _ => return Ok(None),
    };
    let chosen = match network_identity {
        SandboxNetworkIdentity::Offline => users.offline,
        SandboxNetworkIdentity::Online => users.online,
        SandboxNetworkIdentity::CanonicalProof => users.canonical_proof,
    };
    let password = decode_password(&chosen)?;
    Ok(Some(SandboxIdentity {
        username: chosen.username,
        password,
    }))
}

fn normalized_path_identity(path: &Path) -> String {
    path.as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn normalized_path_identities(paths: Vec<PathBuf>) -> Vec<String> {
    let mut identities = paths
        .iter()
        .map(|path| normalized_path_identity(path))
        .collect::<Vec<_>>();
    identities.sort_unstable();
    identities.dedup();
    identities
}

#[allow(clippy::too_many_arguments)]
fn resolve_canonical_windows_sandbox_launch_spec(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<CanonicalWindowsSandboxLaunchSpec> {
    let mut needed_read = read_roots_override
        .map(<[PathBuf]>::to_vec)
        .unwrap_or_else(|| gather_read_roots(command_cwd, permissions, env_map, codex_home));
    extend_read_roots(&mut needed_read, additional_read_roots);
    let needed_write = write_roots_override
        .map(<[PathBuf]>::to_vec)
        .unwrap_or_else(|| gather_write_roots_for_permissions(permissions, command_cwd, env_map));
    let network_identity =
        SandboxNetworkIdentity::from_permissions(permissions, proxy_enforced, env_map);
    if network_identity != SandboxNetworkIdentity::CanonicalProof {
        anyhow::bail!(
            "prepared canonical Windows sandbox launch is missing its private attempt identity"
        );
    }
    let marker = load_marker(codex_home)?;
    let offline_proxy_settings = desired_offline_proxy_settings(
        marker.as_ref(),
        proxy_settings_mode,
        env_map,
        network_identity,
    );
    let request = SandboxSetupRequest {
        permissions,
        command_cwd,
        env_map,
        codex_home,
        proxy_enforced,
    };
    let paths = resolve_sandbox_setup_paths(
        &request,
        &SetupRootOverrides {
            read_roots: Some(needed_read),
            read_roots_include_platform_defaults,
            write_roots: Some(needed_write),
            deny_read_paths: Some(deny_read_paths_override.to_vec()),
            deny_write_paths: Some(deny_write_paths_override.to_vec()),
        },
    );
    Ok(CanonicalWindowsSandboxLaunchSpec {
        permissions: permissions.clone(),
        command_cwd: normalized_path_identity(command_cwd),
        codex_home: normalized_path_identity(codex_home),
        read_roots: normalized_path_identities(paths.read_roots),
        write_roots: normalized_path_identities(paths.write_roots),
        deny_read_paths: normalized_path_identities(paths.deny_read_paths),
        deny_write_paths: normalized_path_identities(paths.deny_write_paths),
        read_roots_include_platform_defaults,
        proxy_enforced,
        proxy_settings_mode,
        network_identity,
        offline_proxy_settings,
    })
}

impl PreparedCanonicalWindowsSandboxLaunch {
    fn new(
        attempt_id: String,
        launch_identity: String,
        spec: CanonicalWindowsSandboxLaunchSpec,
        credentials: SandboxCreds,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(Some(
                PreparedCanonicalWindowsSandboxLaunchState {
                    attempt_id,
                    launch_identity,
                    spec,
                    credentials,
                },
            ))),
        }
    }

    fn consume(
        self,
        attempt_id: &str,
        launch_identity: &str,
        command_cwd: &Path,
        spec: CanonicalWindowsSandboxLaunchSpec,
    ) -> Result<SandboxCreds> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("prepared canonical Windows sandbox launch state was poisoned"))?;
        let prepared = state.take().ok_or_else(|| {
            anyhow!("prepared canonical Windows sandbox launch was already consumed")
        })?;
        let mut mismatches = Vec::new();
        if prepared.attempt_id != attempt_id {
            mismatches.push("attempt identity");
        }
        if prepared.launch_identity != launch_identity {
            mismatches.push("launch identity");
        }
        // Permission profiles can differ only in incidental entry ordering
        // after the shell sandbox transform. Bind to their resolved access
        // semantics for this cwd; the exact effective roots and deny paths are
        // compared separately below.
        if !prepared
            .spec
            .permissions
            .is_semantically_equivalent_to(&spec.permissions, command_cwd)
        {
            mismatches.push("permissions");
        }
        if prepared.spec.command_cwd != spec.command_cwd {
            mismatches.push("command cwd");
        }
        if prepared.spec.codex_home != spec.codex_home {
            mismatches.push("Codex home");
        }
        if prepared.spec.read_roots != spec.read_roots {
            mismatches.push("read roots");
        }
        if prepared.spec.write_roots != spec.write_roots {
            mismatches.push("write roots");
        }
        if prepared.spec.deny_read_paths != spec.deny_read_paths {
            mismatches.push("denied read paths");
        }
        if prepared.spec.deny_write_paths != spec.deny_write_paths {
            mismatches.push("denied write paths");
        }
        if prepared.spec.read_roots_include_platform_defaults
            != spec.read_roots_include_platform_defaults
        {
            mismatches.push("platform read-root policy");
        }
        if prepared.spec.proxy_enforced != spec.proxy_enforced {
            mismatches.push("proxy enforcement");
        }
        if prepared.spec.proxy_settings_mode != spec.proxy_settings_mode {
            mismatches.push("proxy settings mode");
        }
        if prepared.spec.network_identity != spec.network_identity {
            mismatches.push("network identity");
        }
        if prepared.spec.offline_proxy_settings != spec.offline_proxy_settings {
            mismatches.push("offline proxy settings");
        }
        if !mismatches.is_empty() {
            anyhow::bail!(
                "prepared canonical Windows sandbox launch does not match the reserved attempt and exact effective sandbox specification ({})",
                mismatches.join(", ")
            );
        }
        Ok(prepared.credentials)
    }
}

/// Prepare credentials and ACLs once before the canonical watcher handoff.
#[allow(clippy::too_many_arguments)]
pub fn prepare_canonical_windows_sandbox_launch(
    attempt_id: &str,
    launch_identity: &str,
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<PreparedCanonicalWindowsSandboxLaunch> {
    prepare_canonical_windows_sandbox_launch_with_credentials(
        attempt_id,
        launch_identity,
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
        || {
            require_logon_sandbox_creds_with_additional_read_roots(
                permissions,
                command_cwd,
                env_map,
                codex_home,
                read_roots_override,
                additional_read_roots,
                read_roots_include_platform_defaults,
                write_roots_override,
                deny_read_paths_override,
                deny_write_paths_override,
                proxy_enforced,
                proxy_settings_mode,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_canonical_windows_sandbox_launch_with_credentials(
    attempt_id: &str,
    launch_identity: &str,
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    prepare_credentials: impl FnOnce() -> Result<SandboxCreds>,
) -> Result<PreparedCanonicalWindowsSandboxLaunch> {
    if attempt_id.trim().is_empty() || launch_identity.trim().is_empty() {
        anyhow::bail!("canonical Windows sandbox launch binding must not be empty");
    }
    let credentials = prepare_credentials()?;
    // The setup helper runs out of process and may have persisted capability
    // SIDs for a newly allowed per-attempt root. Refresh before the later spawn
    // derives its token SIDs so they match the ACLs the helper just installed.
    refresh_cap_sids_cache_from_disk(codex_home)?;
    // Resolve after setup succeeds so the capability is bound to the settled
    // marker and the exact effective roots the later launch will observe.
    let spec = resolve_canonical_windows_sandbox_launch_spec(
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
    )?;
    Ok(PreparedCanonicalWindowsSandboxLaunch::new(
        attempt_id.to_string(),
        launch_identity.to_string(),
        spec,
        credentials,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn acquire_logon_sandbox_creds_for_launch(
    prepared_launch: Option<PreparedCanonicalWindowsSandboxLaunch>,
    launch_identity: Option<&str>,
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<AcquiredSandboxCreds> {
    acquire_logon_sandbox_creds_for_launch_with_fallback(
        prepared_launch,
        launch_identity,
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
        || {
            require_logon_sandbox_creds_with_additional_read_roots(
                permissions,
                command_cwd,
                env_map,
                codex_home,
                read_roots_override,
                additional_read_roots,
                read_roots_include_platform_defaults,
                write_roots_override,
                deny_read_paths_override,
                deny_write_paths_override,
                proxy_enforced,
                proxy_settings_mode,
            )
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn acquire_logon_sandbox_creds_for_launch_with_fallback(
    prepared_launch: Option<PreparedCanonicalWindowsSandboxLaunch>,
    launch_identity: Option<&str>,
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    fallback: impl FnOnce() -> Result<SandboxCreds>,
) -> Result<AcquiredSandboxCreds> {
    let Some(prepared_launch) = prepared_launch else {
        return Ok(AcquiredSandboxCreds {
            credentials: fallback()?,
            used_prepared_launch: false,
        });
    };
    let launch_identity = launch_identity.ok_or_else(|| {
        anyhow!("prepared canonical Windows sandbox launch is missing its launch identity")
    })?;
    let attempt_id = env_map
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("CODEX_COMPLETION_PROOF_ATTEMPT_ID"))
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("prepared canonical Windows sandbox launch is missing its attempt identity")
        })?;
    let spec = resolve_canonical_windows_sandbox_launch_spec(
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
    )?;
    Ok(AcquiredSandboxCreds {
        credentials: prepared_launch.consume(attempt_id, launch_identity, command_cwd, spec)?,
        used_prepared_launch: true,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn require_logon_sandbox_creds(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<SandboxCreds> {
    require_logon_sandbox_creds_with_additional_read_roots(
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        &[],
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn require_logon_sandbox_creds_with_additional_read_roots(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<SandboxCreds> {
    let sandbox_dir = crate::setup::sandbox_dir(codex_home);
    let mut needed_read = read_roots_override
        .map(<[PathBuf]>::to_vec)
        .unwrap_or_else(|| gather_read_roots(command_cwd, permissions, env_map, codex_home));
    extend_read_roots(&mut needed_read, additional_read_roots);
    let needed_write = write_roots_override
        .map(<[PathBuf]>::to_vec)
        .unwrap_or_else(|| gather_write_roots_for_permissions(permissions, command_cwd, env_map));
    let network_identity =
        SandboxNetworkIdentity::from_permissions(permissions, proxy_enforced, env_map);
    let marker = load_marker(codex_home)?;
    let desired_offline_proxy_settings = desired_offline_proxy_settings(
        marker.as_ref(),
        proxy_settings_mode,
        env_map,
        network_identity,
    );
    // NOTE: Do not add CODEX_HOME/.sandbox to `needed_write`; it must remain non-writable by the
    // restricted capability token. The setup helper's `lock_sandbox_dir` is responsible for
    // granting the sandbox group access to this directory without granting the capability SID.
    let mut setup_reason: Option<String> = None;

    let mut identity = match marker {
        Some(marker) if marker.version_matches() => {
            if let Some(reason) =
                marker.request_mismatch_reason(network_identity, &desired_offline_proxy_settings)
            {
                setup_reason = Some(reason);
                None
            } else {
                let selected = select_identity(network_identity, codex_home)?;
                if selected.is_none() {
                    setup_reason = Some(
                        "sandbox users missing or incompatible with marker version".to_string(),
                    );
                }
                selected
            }
        }
        _ => {
            setup_reason = Some("sandbox setup marker missing or incompatible".to_string());
            None
        }
    };

    if identity.is_none() {
        if let Some(reason) = &setup_reason {
            crate::logging::log_note(
                &format!("sandbox setup required: {reason}"),
                Some(&sandbox_dir),
            );
        } else {
            crate::logging::log_note("sandbox setup required", Some(&sandbox_dir));
        }
        run_elevated_setup_with_proxy_settings(
            crate::setup::SandboxSetupRequest {
                permissions,
                command_cwd,
                env_map,
                codex_home,
                proxy_enforced,
            },
            crate::setup::SetupRootOverrides {
                read_roots: Some(needed_read.clone()),
                read_roots_include_platform_defaults,
                write_roots: Some(needed_write.clone()),
                deny_read_paths: Some(deny_read_paths_override.to_vec()),
                deny_write_paths: Some(deny_write_paths_override.to_vec()),
            },
            &desired_offline_proxy_settings,
        )?;
        identity = select_identity(network_identity, codex_home)?;
    }
    // Always refresh ACLs (non-elevated) for current roots via the setup binary.
    run_setup_refresh_with_overrides_and_proxy_settings(
        crate::setup::SandboxSetupRequest {
            permissions,
            command_cwd,
            env_map,
            codex_home,
            proxy_enforced,
        },
        crate::setup::SetupRootOverrides {
            read_roots: Some(needed_read),
            read_roots_include_platform_defaults,
            write_roots: Some(needed_write),
            deny_read_paths: Some(deny_read_paths_override.to_vec()),
            deny_write_paths: Some(deny_write_paths_override.to_vec()),
        },
        &desired_offline_proxy_settings,
    )?;
    let identity = identity.ok_or_else(|| {
        anyhow!(
            "Windows sandbox setup is missing or out of date; rerun the sandbox setup with elevation"
        )
    })?;
    Ok(SandboxCreds {
        username: identity.username,
        password: identity.password,
    })
}

fn extend_read_roots(read_roots: &mut Vec<PathBuf>, additional_read_roots: &[PathBuf]) {
    for root in additional_read_roots {
        if !read_roots.contains(root) {
            read_roots.push(root.clone());
        }
    }
}

fn desired_offline_proxy_settings(
    marker: Option<&SetupMarker>,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    env_map: &HashMap<String, String>,
    network_identity: SandboxNetworkIdentity,
) -> crate::setup::OfflineProxySettings {
    if network_identity == SandboxNetworkIdentity::CanonicalProof {
        return marker.map_or_else(
            || offline_proxy_settings_from_env(env_map, network_identity),
            SetupMarker::offline_proxy_settings,
        );
    }
    match (marker, proxy_settings_mode) {
        (Some(marker), crate::WindowsSandboxProxySettingsMode::Preserve)
            if marker.version_matches() =>
        {
            marker.offline_proxy_settings()
        }
        _ => offline_proxy_settings_from_env(env_map, network_identity),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn refresh_logon_sandbox_creds(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
) -> Result<SandboxCreds> {
    remove_sandbox_users_file(codex_home, "sandbox user login failed")?;
    require_logon_sandbox_creds_with_additional_read_roots(
        permissions,
        command_cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        write_roots_override,
        deny_read_paths_override,
        deny_write_paths_override,
        proxy_enforced,
        proxy_settings_mode,
    )
}

#[cfg(test)]
mod tests {
    use super::SandboxCreds;
    use super::acquire_logon_sandbox_creds_for_launch_with_fallback;
    use super::desired_offline_proxy_settings;
    use super::extend_read_roots;
    use super::prepare_canonical_windows_sandbox_launch_with_credentials;
    use super::remove_sandbox_users_file;
    use crate::WindowsSandboxProxySettingsMode;
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::setup::SandboxNetworkIdentity;
    use crate::setup::SetupMarker;
    use crate::setup::sandbox_users_path;
    use codex_protocol::models::PermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn additional_read_roots_preserve_base_roots_and_exact_files() {
        let base = PathBuf::from(r"C:\workspace");
        let snapshot = PathBuf::from(r"C:\codex-home\shell_snapshots\session.ps1");
        let mut roots = vec![base.clone()];

        extend_read_roots(&mut roots, &[snapshot.clone(), snapshot.clone()]);

        assert_eq!(roots, vec![base, snapshot]);
    }

    #[test]
    fn remove_sandbox_users_file_deletes_existing_file() {
        let codex_home = TempDir::new().expect("tempdir");
        let users_path = sandbox_users_path(codex_home.path());
        fs::create_dir_all(users_path.parent().expect("sandbox secrets dir"))
            .expect("create sandbox secrets dir");
        fs::write(&users_path, "users").expect("write users");

        remove_sandbox_users_file(codex_home.path(), "stale creds").expect("remove users");
        assert!(!users_path.exists());
    }

    #[test]
    fn remove_sandbox_users_file_ignores_missing_file() {
        let codex_home = TempDir::new().expect("tempdir");
        let users_path = sandbox_users_path(codex_home.path());

        remove_sandbox_users_file(codex_home.path(), "stale creds").expect("remove users");
        assert!(!users_path.exists());
    }

    #[test]
    fn preserving_proxy_settings_uses_the_existing_marker() {
        let marker = SetupMarker {
            version: crate::setup::SETUP_VERSION,
            offline_username: "offline".to_string(),
            online_username: "online".to_string(),
            created_at: None,
            proxy_ports: vec![7890],
            allow_local_binding: true,
        };
        let env_map = HashMap::from([(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:8080".to_string(),
        )]);

        assert_eq!(
            desired_offline_proxy_settings(
                Some(&marker),
                WindowsSandboxProxySettingsMode::Preserve,
                &env_map,
                SandboxNetworkIdentity::Offline,
            ),
            marker.offline_proxy_settings()
        );
        assert_eq!(
            desired_offline_proxy_settings(
                Some(&marker),
                WindowsSandboxProxySettingsMode::Reconcile,
                &env_map,
                SandboxNetworkIdentity::Offline,
            )
            .proxy_ports,
            vec![8080]
        );
    }

    #[test]
    fn canonical_proof_setup_preserves_offline_settings_across_version_upgrade() {
        let marker = SetupMarker {
            version: crate::setup::SETUP_VERSION - 1,
            offline_username: "offline".to_string(),
            online_username: "online".to_string(),
            created_at: None,
            proxy_ports: vec![7890],
            allow_local_binding: false,
        };
        let env_map = HashMap::from([
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:8080".to_string(),
            ),
            (
                "CODEX_NETWORK_ALLOW_LOCAL_BINDING".to_string(),
                "1".to_string(),
            ),
        ]);

        assert_eq!(
            desired_offline_proxy_settings(
                Some(&marker),
                WindowsSandboxProxySettingsMode::Reconcile,
                &env_map,
                SandboxNetworkIdentity::CanonicalProof,
            ),
            marker.offline_proxy_settings()
        );
    }

    #[test]
    fn prepared_canonical_launch_prepares_once_and_never_falls_back_after_handoff() {
        let workspace = TempDir::new().expect("workspace tempdir");
        let codex_home = TempDir::new().expect("codex home tempdir");
        let workspace_root = AbsolutePathBuf::from_absolute_path(workspace.path().to_path_buf())
            .expect("absolute workspace root");
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &PermissionProfile::workspace_write(),
                std::slice::from_ref(&workspace_root),
            )
            .expect("resolved permissions");
        let env_map = HashMap::from([(
            "CODEX_COMPLETION_PROOF_ATTEMPT_ID".to_string(),
            "attempt-1".to_string(),
        )]);
        let read_roots = vec![workspace.path().to_path_buf()];
        let write_roots = vec![workspace.path().to_path_buf()];
        let preparations = Cell::new(0);
        let post_handoff_fallbacks = Cell::new(0);

        let prepared = prepare_canonical_windows_sandbox_launch_with_credentials(
            "attempt-1",
            "just completion-proof",
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || {
                preparations.set(preparations.get() + 1);
                Ok(SandboxCreds {
                    username: "prepared-user".to_string(),
                    password: "prepared-password".to_string(),
                })
            },
        )
        .expect("prepare canonical launch");
        let replay = prepared.clone();

        let acquired = acquire_logon_sandbox_creds_for_launch_with_fallback(
            Some(prepared),
            Some("just completion-proof"),
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || {
                post_handoff_fallbacks.set(post_handoff_fallbacks.get() + 1);
                Ok(SandboxCreds {
                    username: "fallback-user".to_string(),
                    password: "fallback-password".to_string(),
                })
            },
        )
        .expect("consume prepared launch");

        assert_eq!(1, preparations.get());
        assert_eq!(0, post_handoff_fallbacks.get());
        assert!(acquired.used_prepared_launch);
        assert_eq!("prepared-user", acquired.credentials.username);

        let error = acquire_logon_sandbox_creds_for_launch_with_fallback(
            Some(replay),
            Some("just completion-proof"),
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || {
                post_handoff_fallbacks.set(post_handoff_fallbacks.get() + 1);
                Ok(SandboxCreds {
                    username: "fallback-user".to_string(),
                    password: "fallback-password".to_string(),
                })
            },
        )
        .expect_err("prepared launch must be single use");
        assert!(error.to_string().contains("already consumed"));
        assert_eq!(0, post_handoff_fallbacks.get());
    }

    #[test]
    fn prepared_canonical_launch_spec_mismatch_burns_the_capability() {
        let workspace = TempDir::new().expect("workspace tempdir");
        let codex_home = TempDir::new().expect("codex home tempdir");
        let workspace_root = AbsolutePathBuf::from_absolute_path(workspace.path().to_path_buf())
            .expect("absolute workspace root");
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &PermissionProfile::workspace_write(),
                std::slice::from_ref(&workspace_root),
            )
            .expect("resolved permissions");
        let env_map = HashMap::from([(
            "CODEX_COMPLETION_PROOF_ATTEMPT_ID".to_string(),
            "attempt-2".to_string(),
        )]);
        let read_roots = vec![workspace.path().to_path_buf()];
        let write_roots = vec![workspace.path().to_path_buf()];
        let mismatched_write_roots = vec![workspace.path().join("different-write-root")];
        let prepared = prepare_canonical_windows_sandbox_launch_with_credentials(
            "attempt-2",
            "just completion-proof",
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || {
                Ok(SandboxCreds {
                    username: "prepared-user".to_string(),
                    password: "prepared-password".to_string(),
                })
            },
        )
        .expect("prepare canonical launch");
        let replay = prepared.clone();

        let mismatch = acquire_logon_sandbox_creds_for_launch_with_fallback(
            Some(prepared),
            Some("just completion-proof"),
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&mismatched_write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || unreachable!("prepared launch mismatch must not refresh credentials"),
        )
        .expect_err("changed effective roots must be rejected");
        assert!(mismatch.to_string().contains("does not match"));

        let replay_error = acquire_logon_sandbox_creds_for_launch_with_fallback(
            Some(replay),
            Some("just completion-proof"),
            &permissions,
            workspace.path(),
            &env_map,
            codex_home.path(),
            Some(&read_roots),
            &[],
            false,
            Some(&write_roots),
            &[],
            &[],
            false,
            WindowsSandboxProxySettingsMode::Reconcile,
            || unreachable!("prepared launch replay must not refresh credentials"),
        )
        .expect_err("mismatch must consume the capability");
        assert!(replay_error.to_string().contains("already consumed"));
    }
}
