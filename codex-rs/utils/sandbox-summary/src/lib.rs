use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::NetworkAccess;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Describes effective permissions for display; this never enforces them.
///
/// Workspace-write summaries list the runtime workspace roots rather than the
/// profile's legacy writable roots, which can include Codex-internal paths.
pub fn summarize_permission_profile(
    permission_profile: &PermissionProfile,
    cwd: &AbsolutePathBuf,
    workspace_roots: &[AbsolutePathBuf],
) -> String {
    let (mut summary, network_access) =
        match permission_profile.to_legacy_sandbox_policy(cwd.as_path()) {
            Ok(SandboxPolicy::DangerFullAccess) => return "danger-full-access".to_string(),
            Ok(SandboxPolicy::ReadOnly { network_access, .. }) => {
                ("read-only".to_string(), network_access)
            }
            Ok(SandboxPolicy::ExternalSandbox { network_access }) => (
                "external-sandbox".to_string(),
                matches!(network_access, NetworkAccess::Enabled),
            ),
            Ok(SandboxPolicy::WorkspaceWrite {
                network_access,
                exclude_tmpdir_env_var,
                ..
            }) => {
                let mut summary = "workspace-write [workdir".to_string();
                if !exclude_tmpdir_env_var {
                    summary.push_str(", $TMPDIR");
                }
                for root in workspace_roots.iter().filter(|root| *root != cwd) {
                    summary.push_str(", ");
                    summary.push_str(&root.to_string_lossy());
                }
                summary.push(']');
                (summary, network_access)
            }
            Err(_) => (
                "custom permissions".to_string(),
                permission_profile.network_sandbox_policy().is_enabled(),
            ),
        };
    if network_access {
        summary.push_str(" (network access enabled)");
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSandboxPolicy;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn summarizes_non_workspace_profiles_with_network_suffix() {
        let base = std::env::current_dir().unwrap();
        let cwd = AbsolutePathBuf::try_from(base.join("repo")).unwrap();
        let outside_root = AbsolutePathBuf::try_from(base.join("outside")).unwrap();
        let outside_write = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Path { path: outside_root },
            access: FileSystemAccessMode::Write,
        }]);
        for (profile, expected) in [
            (PermissionProfile::Disabled, "danger-full-access"),
            (
                PermissionProfile::External {
                    network: NetworkSandboxPolicy::Restricted,
                },
                "external-sandbox",
            ),
            (
                PermissionProfile::External {
                    network: NetworkSandboxPolicy::Enabled,
                },
                "external-sandbox (network access enabled)",
            ),
            (
                PermissionProfile::from_runtime_permissions(
                    &FileSystemSandboxPolicy::read_only(),
                    NetworkSandboxPolicy::Enabled,
                ),
                "read-only (network access enabled)",
            ),
            (
                PermissionProfile::from_runtime_permissions(
                    &outside_write,
                    NetworkSandboxPolicy::Restricted,
                ),
                "custom permissions",
            ),
            (
                PermissionProfile::from_runtime_permissions(
                    &outside_write,
                    NetworkSandboxPolicy::Enabled,
                ),
                "custom permissions (network access enabled)",
            ),
        ] {
            assert_eq!(
                summarize_permission_profile(&profile, &cwd, std::slice::from_ref(&cwd)),
                expected
            );
        }
    }

    #[test]
    fn permission_profile_summary_uses_runtime_workspace_roots_and_hides_internal_writes() {
        let base = std::env::current_dir().unwrap();
        let cwd = AbsolutePathBuf::try_from(base.join("repo")).unwrap();
        let extra_root = AbsolutePathBuf::try_from(base.join("repo-extra")).unwrap();
        let hidden_root = AbsolutePathBuf::try_from(base.join(".codex/memories")).unwrap();
        let profile = PermissionProfile::workspace_write_with(
            std::slice::from_ref(&hidden_root),
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ false,
            /*exclude_slash_tmp*/ false,
        );

        let summary =
            summarize_permission_profile(&profile, &cwd, &[cwd.clone(), extra_root.clone()]);

        assert_eq!(
            summary,
            format!(
                "workspace-write [workdir, $TMPDIR, {}]",
                extra_root.display()
            )
        );
    }

    #[test]
    fn workspace_write_summary_includes_network_access_and_ignores_legacy_slash_tmp() {
        let cwd = AbsolutePathBuf::try_from(std::env::current_dir().unwrap()).unwrap();
        for exclude_slash_tmp in [false, true] {
            let profile = PermissionProfile::workspace_write_with(
                &[],
                NetworkSandboxPolicy::Enabled,
                /*exclude_tmpdir_env_var*/ true,
                exclude_slash_tmp,
            );
            assert_eq!(
                summarize_permission_profile(&profile, &cwd, std::slice::from_ref(&cwd)),
                "workspace-write [workdir] (network access enabled)"
            );
        }
    }
}
