use crate::acl::add_deny_write_ace;
use crate::acl::ensure_allow_write_aces;
use crate::allow::AllowDenyPaths;
use crate::allow::compute_allow_paths_for_permissions;
use crate::cap::load_or_create_cap_sids;
use crate::cap::workspace_write_cap_sid_for_root_keys;
use crate::cap::workspace_write_root_contains_path;
use crate::cap::workspace_write_root_overlaps_path;
use crate::cap::workspace_write_root_specificity;
use crate::deny_read_state::sync_persistent_deny_read_acls;
use crate::env::apply_no_network_to_env;
use crate::env::ensure_non_interactive_pager;
use crate::env::inherit_path_env;
use crate::env::normalize_null_device_env;
use crate::identity::SandboxCreds;
use crate::identity::require_logon_sandbox_creds_with_additional_read_roots;
use crate::logging::log_start;
use crate::path_normalization::security_path_key as canonical_path_key;
use crate::path_normalization::canonicalize_path;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::sandbox_utils::ensure_codex_home_exists;
use crate::sandbox_utils::inject_git_safe_directory;
use crate::setup::effective_write_roots_for_permissions;
use crate::token::LocalSid;
use crate::token::create_readonly_token_with_cap;
use crate::token::create_workspace_write_token_with_caps_from;
use crate::token::get_current_token_for_restriction;
use crate::workspace_acl::is_command_cwd_root;
use crate::workspace_acl::protect_workspace_agents_dir;
use crate::workspace_acl::protect_workspace_codex_dir;
use anyhow::Context;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;

pub(crate) struct SpawnContext {
    pub(crate) permissions: ResolvedWindowsSandboxPermissions,
    pub(crate) current_dir: PathBuf,
    pub(crate) logs_base_dir: Option<PathBuf>,
    pub(crate) uses_write_capabilities: bool,
}

pub(crate) struct ElevatedSpawnContext {
    pub(crate) sandbox_base: PathBuf,
    pub(crate) logs_base_dir: Option<PathBuf>,
    pub(crate) sandbox_creds: SandboxCreds,
    pub(crate) cap_sids: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SpawnPrepOptions {
    pub(crate) inherit_path: bool,
    pub(crate) add_git_safe_directory: bool,
}

pub(crate) struct LegacySessionSecurity {
    pub(crate) h_token: HANDLE,
    pub(crate) readonly_sid: Option<LocalSid>,
    pub(crate) readonly_sid_str: Option<String>,
    pub(crate) write_root_sids: Vec<RootCapabilitySid>,
}

pub(crate) struct RootCapabilitySid {
    pub(crate) root: PathBuf,
    pub(crate) sid: LocalSid,
    pub(crate) sid_str: String,
}

pub(crate) struct LegacyAclSids<'a> {
    pub(crate) readonly_sid: Option<&'a LocalSid>,
    pub(crate) readonly_sid_str: Option<&'a str>,
    pub(crate) write_root_sids: &'a [RootCapabilitySid],
}

fn prepare_spawn_context_common(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
    command: &[String],
    options: SpawnPrepOptions,
) -> Result<SpawnContext> {
    crate::env::normalize_environment(env_map)?;
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;

    normalize_null_device_env(env_map);
    ensure_non_interactive_pager(env_map);
    if options.inherit_path {
        inherit_path_env(env_map);
    }
    if options.add_git_safe_directory {
        inject_git_safe_directory(env_map, cwd);
    }

    ensure_codex_home_exists(codex_home)?;
    let sandbox_base = codex_home.join(".sandbox");
    std::fs::create_dir_all(&sandbox_base)?;
    let logs_base_dir = Some(sandbox_base);
    log_start(command, logs_base_dir.as_deref());

    let uses_write_capabilities = permissions.uses_write_capabilities_for_cwd(cwd, env_map);

    Ok(SpawnContext {
        permissions,
        current_dir: cwd.to_path_buf(),
        logs_base_dir,
        uses_write_capabilities,
    })
}

pub(crate) fn prepare_legacy_spawn_context(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    codex_home: &Path,
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
    command: &[String],
    options: SpawnPrepOptions,
) -> Result<SpawnContext> {
    let common = prepare_spawn_context_common(
        permission_profile,
        workspace_roots,
        codex_home,
        cwd,
        env_map,
        command,
        options,
    )?;
    if common.permissions.should_apply_network_block() {
        apply_no_network_to_env(env_map)?;
    }
    Ok(common)
}

pub(crate) fn prepare_legacy_session_security(
    uses_write_capabilities: bool,
    codex_home: &Path,
    cwd: &Path,
    capability_roots: impl IntoIterator<Item = PathBuf>,
) -> Result<LegacySessionSecurity> {
    let caps = load_or_create_cap_sids(codex_home)?;
    // SAFETY: LocalSid owners retain every capability SID through token construction. The base
    // token is closed before propagating construction failure, and the successful
    // restricted token transfers to LegacySessionSecurity.
    let (h_token, readonly_sid, readonly_sid_str, write_root_sids) = unsafe {
        if uses_write_capabilities {
            let write_root_sids = root_capability_sids(codex_home, cwd, capability_roots)?;
            if write_root_sids.is_empty() {
                anyhow::bail!("workspace-write sandbox has no writable root capability SIDs");
            }
            let base = get_current_token_for_restriction()?;
            let cap_ptrs: Vec<*mut c_void> = write_root_sids
                .iter()
                .map(|root| root.sid.as_ptr())
                .collect();
            let h_token = create_workspace_write_token_with_caps_from(base, cap_ptrs.as_slice());
            CloseHandle(base);
            let h_token = h_token?;
            (h_token, None, None, write_root_sids)
        } else {
            let psid = LocalSid::from_string(&caps.readonly)?;
            let (h_token, _psid) = create_readonly_token_with_cap(psid.as_ptr())?;
            (h_token, Some(psid), Some(caps.readonly), Vec::new())
        }
    };

    Ok(LegacySessionSecurity {
        h_token,
        readonly_sid,
        readonly_sid_str,
        write_root_sids,
    })
}

pub(crate) fn legacy_session_capability_roots(
    permissions: &ResolvedWindowsSandboxPermissions,
    current_dir: &Path,
    env_map: &HashMap<String, String>,
    codex_home: &Path,
) -> Vec<PathBuf> {
    let allow_paths = compute_allow_paths_for_permissions(permissions, current_dir, env_map)
        .allow
        .into_iter()
        .collect::<Vec<_>>();
    if permissions.uses_write_capabilities_for_cwd(current_dir, env_map) {
        effective_write_roots_for_permissions(
            permissions,
            current_dir,
            env_map,
            codex_home,
            Some(allow_paths.as_slice()),
        )
    } else {
        allow_paths
    }
}

pub(crate) fn root_capability_sids(
    codex_home: &Path,
    cwd: &Path,
    allow_paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<RootCapabilitySid>> {
    let cwd_key = canonical_path_key(cwd);
    let mut roots = allow_paths
        .into_iter()
        .map(|root| (canonical_path_key(&root), root))
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| left.0.cmp(&right.0));
    roots.dedup_by(|left, right| left.0 == right.0);

    let mut out = Vec::with_capacity(roots.len());
    for (root_key, root) in roots {
        let sid_str = workspace_write_cap_sid_for_root_keys(codex_home, &cwd_key, root_key)?;
        let sid = LocalSid::from_string(&sid_str)?;
        out.push(RootCapabilitySid { root, sid, sid_str });
    }
    Ok(out)
}

pub(crate) fn sandbox_capability_sid_strings(
    uses_write_capabilities: bool,
    write_root_sids: Vec<String>,
    readonly_sid: &str,
) -> Vec<String> {
    if uses_write_capabilities && !write_root_sids.is_empty() {
        write_root_sids
    } else {
        vec![readonly_sid.to_string()]
    }
}

fn matching_root_capability<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Option<&'a RootCapabilitySid> {
    root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_contains_path(&root_sid.root, path))
        .max_by_key(|root_sid| workspace_write_root_specificity(&root_sid.root))
}

fn deny_root_capabilities_for_path<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Vec<&'a RootCapabilitySid> {
    let matching_root_sids = root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_overlaps_path(&root_sid.root, path))
        .collect::<Vec<_>>();
    if matching_root_sids.is_empty() {
        root_sids.iter().collect()
    } else {
        matching_root_sids
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_legacy_session_acl_rules(
    permissions: &ResolvedWindowsSandboxPermissions,
    codex_home: &Path,
    current_dir: &Path,
    env_map: &HashMap<String, String>,
    additional_deny_read_paths: &[PathBuf],
    additional_deny_write_paths: &[PathBuf],
    acl_sids: LegacyAclSids<'_>,
) -> Result<()> {
    let AllowDenyPaths { allow, mut deny } =
        compute_allow_paths_for_permissions(permissions, current_dir, env_map);
    crate::setup::ensure_safe_write_roots(&allow, codex_home)?;
    // SAFETY: LegacyAclSids borrows LocalSid owners that outlive these synchronous ACL operations,
    // so the readonly and root-capability pointers remain valid throughout each call.
    unsafe {
        for path in additional_deny_write_paths {
            // Explicit carveouts must exist before the command starts so the
            // sandbox cannot create them under a writable parent first.
            if !path.exists() {
                std::fs::create_dir_all(path)
                    .with_context(|| format!("create deny-write path {}", path.display()))?;
            }
            deny.insert(path.clone());
        }
        // Only write-root capabilities receive allow ACEs. A read-only token has no writable roots,
        // and a write-restricted token's reads never consult its capability SIDs.
        for p in &allow {
            let Some(root_sid) = matching_root_capability(p, acl_sids.write_root_sids) else {
                continue;
            };
            ensure_allow_write_aces(p, &[root_sid.sid.as_ptr()])
                .with_context(|| format!("add required write ACL for {}", p.display()))?;
        }
        for p in &deny {
            for root_sid in deny_root_capabilities_for_path(p, acl_sids.write_root_sids) {
                add_deny_write_ace(p, root_sid.sid.as_ptr())
                    .with_context(|| format!("add required deny-write ACL for {}", p.display()))?;
            }
        }
        if let Some(readonly_sid) = acl_sids.readonly_sid {
            let Some(readonly_sid_str) = acl_sids.readonly_sid_str else {
                anyhow::bail!("readonly capability SID string missing");
            };
            sync_persistent_deny_read_acls(
                codex_home,
                readonly_sid_str,
                additional_deny_read_paths,
                readonly_sid.as_ptr(),
            )?;
        } else {
            for root_sid in acl_sids.write_root_sids {
                sync_persistent_deny_read_acls(
                    codex_home,
                    &root_sid.sid_str,
                    additional_deny_read_paths,
                    root_sid.sid.as_ptr(),
                )?;
            }
        }
        if !acl_sids.write_root_sids.is_empty()
            && let Some(workspace_sid) =
                matching_root_capability(current_dir, acl_sids.write_root_sids)
        {
            let canonical_cwd = canonicalize_path(current_dir);
            if is_command_cwd_root(&workspace_sid.root, &canonical_cwd) {
                protect_workspace_codex_dir(current_dir, workspace_sid.sid.as_ptr()).with_context(
                    || {
                        format!(
                            "protect workspace .codex directory under {}",
                            current_dir.display()
                        )
                    },
                )?;
                protect_workspace_agents_dir(current_dir, workspace_sid.sid.as_ptr())
                    .with_context(|| {
                        format!(
                            "protect workspace .agents directory under {}",
                            current_dir.display()
                        )
                    })?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_elevated_spawn_context_for_permissions(
    permissions: ResolvedWindowsSandboxPermissions,
    codex_home: &Path,
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
    command: &[String],
    read_roots_override: Option<&[PathBuf]>,
    additional_read_roots: &[PathBuf],
    read_roots_include_platform_defaults: bool,
    write_roots_override: Option<&[PathBuf]>,
    deny_read_paths_override: &[PathBuf],
    deny_write_paths_override: &[PathBuf],
    proxy_enforced: bool,
    proxy_settings_mode: crate::WindowsSandboxProxySettingsMode,
    cancellation: Option<&crate::WindowsSandboxCancellationToken>,
) -> Result<ElevatedSpawnContext> {
    crate::setup::check_setup_cancellation(cancellation)?;
    crate::env::normalize_environment(env_map)?;
    normalize_null_device_env(env_map);
    ensure_non_interactive_pager(env_map);
    inherit_path_env(env_map);
    inject_git_safe_directory(env_map, cwd);

    // Use a temp-based log dir that the sandbox user can write.
    let sandbox_base = codex_home.join(".sandbox");
    ensure_codex_home_exists(&sandbox_base)?;
    let logs_base_dir = Some(sandbox_base.clone());
    log_start(command, logs_base_dir.as_deref());

    let uses_write_capabilities = permissions.uses_write_capabilities_for_cwd(cwd, env_map);

    let AllowDenyPaths { allow, deny } =
        compute_allow_paths_for_permissions(&permissions, cwd, env_map);
    crate::setup::ensure_safe_write_roots(&allow, codex_home)?;
    let write_roots: Vec<PathBuf> = allow.into_iter().collect();
    let deny_write_paths: Vec<PathBuf> = deny.into_iter().collect();
    let computed_write_roots_override = if uses_write_capabilities {
        Some(write_roots.as_slice())
    } else {
        None
    };
    let write_roots_for_setup = write_roots_override.or(computed_write_roots_override);
    let effective_write_roots = if uses_write_capabilities {
        effective_write_roots_for_permissions(
            &permissions,
            cwd,
            env_map,
            codex_home,
            write_roots_for_setup,
        )
    } else {
        Vec::new()
    };
    let setup_write_roots_override = if uses_write_capabilities {
        Some(effective_write_roots.as_slice())
    } else {
        write_roots_override
    };
    let sandbox_creds = require_logon_sandbox_creds_with_additional_read_roots(
        &permissions,
        cwd,
        env_map,
        codex_home,
        read_roots_override,
        additional_read_roots,
        read_roots_include_platform_defaults,
        setup_write_roots_override,
        deny_read_paths_override,
        if deny_write_paths_override.is_empty() {
            &deny_write_paths
        } else {
            deny_write_paths_override
        },
        proxy_enforced,
        proxy_settings_mode,
        cancellation,
    )?;
    let caps = load_or_create_cap_sids(codex_home)?;
    let write_root_sids = if uses_write_capabilities {
        root_capability_sids(codex_home, cwd, effective_write_roots)?
            .into_iter()
            .map(|root_sid| root_sid.sid_str)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let cap_sids =
        sandbox_capability_sid_strings(uses_write_capabilities, write_root_sids, &caps.readonly);

    Ok(ElevatedSpawnContext {
        sandbox_base,
        logs_base_dir,
        sandbox_creds,
        cap_sids,
    })
}

