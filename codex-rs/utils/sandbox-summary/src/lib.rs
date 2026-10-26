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

