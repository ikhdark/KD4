use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::protocol::WritableRoot;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::SandboxType;
use crate::compatibility_sandbox_policy_for_permission_profile;

/// Resolved filesystem overrides for the Windows sandbox backends.
///
/// The elevated Windows backend consumes extra deny-read paths plus explicit
/// read and write roots during setup/refresh. The unelevated restricted-token
/// backend derives write roots from the permission profile and consumes extra
/// deny-write carveouts that need preparation before launch. Complete readable-root
/// allowlists are refused: shared sandbox accounts retain additive read grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSandboxFilesystemOverrides {
    pub read_roots_override: Option<Vec<PathBuf>>,
    pub read_roots_include_platform_defaults: bool,
    pub write_roots_override: Option<Vec<PathBuf>>,
    pub additional_deny_read_paths: Vec<AbsolutePathBuf>,
    pub additional_deny_write_paths: Vec<AbsolutePathBuf>,
}

pub fn permission_profile_supports_windows_restricted_token_sandbox(
    permission_profile: &PermissionProfile,
) -> bool {
    match permission_profile {
        PermissionProfile::Managed { file_system, .. } => {
            !file_system.to_sandbox_policy().has_full_disk_write_access()
        }
        PermissionProfile::Disabled | PermissionProfile::External { .. } => false,
    }
}

pub fn unsupported_windows_restricted_token_sandbox_reason(
    sandbox: SandboxType,
    permission_profile: &PermissionProfile,
    sandbox_policy_cwd: &AbsolutePathBuf,
    windows_sandbox_level: WindowsSandboxLevel,
) -> Option<String> {
    if windows_sandbox_level == WindowsSandboxLevel::Elevated {
        resolve_windows_elevated_filesystem_overrides(
            sandbox,
            permission_profile,
            sandbox_policy_cwd,
            windows_sandbox_level == WindowsSandboxLevel::Elevated,
        )
        .err()
    } else {
        resolve_windows_restricted_token_filesystem_overrides(
            sandbox,
            permission_profile,
            sandbox_policy_cwd,
            windows_sandbox_level,
        )
        .err()
    }
}

pub fn resolve_windows_restricted_token_filesystem_overrides(
    sandbox: SandboxType,
    permission_profile: &PermissionProfile,
    sandbox_policy_cwd: &AbsolutePathBuf,
    windows_sandbox_level: WindowsSandboxLevel,
) -> std::result::Result<Option<WindowsSandboxFilesystemOverrides>, String> {
    if sandbox != SandboxType::WindowsRestrictedToken
        || windows_sandbox_level == WindowsSandboxLevel::Elevated
    {
        return Ok(None);
    }

    let (file_system_sandbox_policy, network_sandbox_policy) =
        permission_profile.to_runtime_permissions();

    let needs_direct_runtime_enforcement = file_system_sandbox_policy
        .needs_direct_runtime_enforcement(network_sandbox_policy, sandbox_policy_cwd);

    if !permission_profile_supports_windows_restricted_token_sandbox(permission_profile) {
        let permission_profile_name = permission_profile_display_name(permission_profile);
        return Err(format!(
            "windows sandbox backend cannot enforce file_system={:?}, network={network_sandbox_policy:?}, permission_profile={permission_profile_name}; refusing to run unsandboxed",
            file_system_sandbox_policy.kind,
        ));
    }

    // The restricted-token backend can still enforce split write restrictions,
    // but its WRITE_RESTRICTED token does not make capability SID deny-read ACEs
    // participate in read access checks. Complete read allowlists are unsupported
    // by either backend; explicit deny-read entries require the elevated backend.
    if !windows_policy_has_root_read_access(&file_system_sandbox_policy, sandbox_policy_cwd) {
        return Err(
            "windows sandbox cannot enforce a complete readable-root allowlist with shared sandbox accounts; refusing to run unsandboxed"
                .to_string(),
        );
    }

    if !needs_direct_runtime_enforcement {
        return Ok(None);
    }

    let additional_deny_read_paths = codex_windows_sandbox::resolve_windows_deny_read_paths(
        &file_system_sandbox_policy,
        sandbox_policy_cwd,
    )?;
    if !additional_deny_read_paths.is_empty() {
        return Err(
            "windows unelevated restricted-token sandbox cannot enforce deny-read restrictions directly; refusing to run unsandboxed"
                .to_string(),
        );
    }

    let legacy_projection = compatibility_sandbox_policy_for_permission_profile(
        permission_profile,
        sandbox_policy_cwd.as_path(),
    );
    let legacy_writable_roots = legacy_projection.get_writable_roots_with_cwd(sandbox_policy_cwd);
    let split_writable_roots =
        file_system_sandbox_policy.get_writable_roots_with_cwd(sandbox_policy_cwd);
    // The backend derives capability SIDs and write ACLs from this permission
    // profile directly. The compatibility projection can add a writable cwd,
    // so its root set must not constrain profiles that keep the cwd read-only.
    if has_reopened_writable_descendant(&split_writable_roots) {
        return Err(
            "windows unelevated restricted-token sandbox cannot reopen writable descendants under read-only carveouts directly; refusing to run unsandboxed"
                .to_string(),
        );
    }

    let mut additional_deny_write_paths = BTreeSet::new();
    for split_root in &split_writable_roots {
        let split_root_path = normalize_windows_override_path(split_root.root.as_path())?;
        let legacy_root = legacy_writable_roots.iter().find(|candidate| {
            normalize_windows_override_path(candidate.root.as_path())
                .is_ok_and(|candidate_path| candidate_path == split_root_path)
        });

        for read_only_subpath in &split_root.read_only_subpaths {
            if !legacy_root.is_some_and(|root| {
                root.read_only_subpaths
                    .iter()
                    .any(|candidate| candidate == read_only_subpath)
            }) {
                additional_deny_write_paths.insert(normalize_windows_override_path(
                    read_only_subpath.as_path(),
                )?);
            }
        }
    }

    if additional_deny_read_paths.is_empty() && additional_deny_write_paths.is_empty() {
        return Ok(None);
    }

    Ok(Some(WindowsSandboxFilesystemOverrides {
        read_roots_override: None,
        read_roots_include_platform_defaults: false,
        write_roots_override: None,
        additional_deny_read_paths,
        additional_deny_write_paths: additional_deny_write_paths
            .into_iter()
            .map(|path| AbsolutePathBuf::from_absolute_path(path).map_err(|err| err.to_string()))
            .collect::<std::result::Result<_, _>>()?,
    }))
}

pub fn resolve_windows_elevated_filesystem_overrides(
    sandbox: SandboxType,
    permission_profile: &PermissionProfile,
    sandbox_policy_cwd: &AbsolutePathBuf,
    use_windows_elevated_backend: bool,
) -> std::result::Result<Option<WindowsSandboxFilesystemOverrides>, String> {
    if sandbox != SandboxType::WindowsRestrictedToken || !use_windows_elevated_backend {
        return Ok(None);
    }

    let (file_system_sandbox_policy, network_sandbox_policy) =
        permission_profile.to_runtime_permissions();

    if !permission_profile_supports_windows_restricted_token_sandbox(permission_profile) {
        let permission_profile_name = permission_profile_display_name(permission_profile);
        return Err(format!(
            "windows sandbox backend cannot enforce file_system={:?}, network={network_sandbox_policy:?}, permission_profile={permission_profile_name}; refusing to run unsandboxed",
            file_system_sandbox_policy.kind,
        ));
    }

    let additional_deny_read_paths = codex_windows_sandbox::resolve_windows_deny_read_paths(
        &file_system_sandbox_policy,
        sandbox_policy_cwd,
    )?;

    let split_writable_roots =
        file_system_sandbox_policy.get_writable_roots_with_cwd(sandbox_policy_cwd);
    if has_reopened_writable_descendant(&split_writable_roots) {
        return Err(
            "windows elevated sandbox cannot reopen writable descendants under read-only carveouts directly; refusing to run unsandboxed"
                .to_string(),
        );
    }

    let needs_direct_runtime_enforcement = file_system_sandbox_policy
        .needs_direct_runtime_enforcement(network_sandbox_policy, sandbox_policy_cwd);
    let normalize_path = |path: PathBuf| dunce::canonicalize(&path).unwrap_or(path);
    let legacy_projection = compatibility_sandbox_policy_for_permission_profile(
        permission_profile,
        sandbox_policy_cwd.as_path(),
    );
    let legacy_writable_roots = legacy_projection.get_writable_roots_with_cwd(sandbox_policy_cwd);
    let normalized_legacy_roots: Vec<_> = legacy_writable_roots
        .iter()
        .map(|root| (normalize_path(root.root.to_path_buf()), root))
        .collect();
    let legacy_root_paths: BTreeSet<&PathBuf> = normalized_legacy_roots
        .iter()
        .map(|(path, _)| path)
        .collect();
    let split_root_paths: Vec<PathBuf> = split_writable_roots
        .iter()
        .map(|root| normalize_path(root.root.to_path_buf()))
        .collect();
    let split_root_path_set: BTreeSet<&PathBuf> = split_root_paths.iter().collect();

    // Explicit deny-read entries still use the elevated deny owner. Positive
    // read allowlists cannot be isolated from persistent shared-account grants.
    if !windows_policy_has_root_read_access(&file_system_sandbox_policy, sandbox_policy_cwd) {
        return Err(
            "windows sandbox cannot enforce a complete readable-root allowlist with shared sandbox accounts; refusing to run unsandboxed".to_string(),
        );
    }

    let write_roots_override = if split_root_path_set == legacy_root_paths {
        None
    } else {
        Some(split_root_paths.clone())
    };

    let additional_deny_write_paths = if needs_direct_runtime_enforcement {
        let mut deny_paths = BTreeSet::new();
        for (writable_root, writable_root_path) in
            split_writable_roots.iter().zip(&split_root_paths)
        {
            let legacy_root = normalized_legacy_roots
                .iter()
                .find(|(path, _)| path == writable_root_path)
                .map(|(_, root)| *root);
            for read_only_subpath in &writable_root.read_only_subpaths {
                let read_only_subpath_suffix = read_only_subpath
                    .as_path()
                    .strip_prefix(writable_root.root.as_path())
                    .ok();
                let already_denied_by_legacy = legacy_root.is_some_and(|legacy_root| {
                    legacy_root.read_only_subpaths.iter().any(|candidate| {
                        candidate
                            .as_path()
                            .strip_prefix(legacy_root.root.as_path())
                            .ok()
                            == read_only_subpath_suffix
                    })
                });
                if !already_denied_by_legacy {
                    deny_paths.insert(normalize_path(read_only_subpath.to_path_buf()));
                }
            }
        }
        deny_paths
            .into_iter()
            .map(|path| AbsolutePathBuf::from_absolute_path(path).map_err(|err| err.to_string()))
            .collect::<std::result::Result<_, _>>()?
    } else {
        Vec::new()
    };

    if write_roots_override.is_none()
        && additional_deny_read_paths.is_empty()
        && additional_deny_write_paths.is_empty()
    {
        return Ok(None);
    }

    Ok(Some(WindowsSandboxFilesystemOverrides {
        read_roots_include_platform_defaults: false,
        read_roots_override: None,
        write_roots_override,
        additional_deny_read_paths,
        additional_deny_write_paths,
    }))
}

fn normalize_windows_override_path(path: &Path) -> std::result::Result<PathBuf, String> {
    AbsolutePathBuf::from_absolute_path(dunce::simplified(path))
        .map(AbsolutePathBuf::into_path_buf)
        .map_err(|err| err.to_string())
}

fn windows_policy_has_root_read_access(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> bool {
    codex_windows_sandbox::validate_windows_read_policy(file_system_sandbox_policy, cwd.as_path())
        .is_ok()
}

fn permission_profile_display_name(permission_profile: &PermissionProfile) -> &'static str {
    match permission_profile {
        PermissionProfile::Managed { .. } => "Managed",
        PermissionProfile::Disabled => "Disabled",
        PermissionProfile::External { .. } => "External",
    }
}

fn has_reopened_writable_descendant(writable_roots: &[WritableRoot]) -> bool {
    writable_roots.iter().any(|writable_root| {
        writable_root
            .read_only_subpaths
            .iter()
            .any(|read_only_subpath| {
                writable_roots.iter().any(|candidate| {
                    candidate.root.as_path() != writable_root.root.as_path()
                        && candidate
                            .root
                            .as_path()
                            .starts_with(read_only_subpath.as_path())
                })
            })
    })
}
