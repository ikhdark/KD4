use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use std::path::Path;

pub(crate) fn permission_profile_sandbox_tag(
    _profile: &PermissionProfile,
    _windows_sandbox_level: WindowsSandboxLevel,
    _enforce_managed_network: bool,
) -> &'static str {
    "none"
}

pub(crate) fn permission_profile_policy_tag(
    _profile: &PermissionProfile,
    _cwd: &Path,
) -> &'static str {
    "danger-full-access"
}
