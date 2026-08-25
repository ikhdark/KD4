use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_READ_ONLY;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;

#[derive(Debug, Clone)]
pub(crate) struct ApprovalPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub approval: AskForApproval,
    pub active_permission_profile: ActivePermissionProfile,
    pub permission_profile: PermissionProfile,
}

pub(crate) fn builtin_approval_presets() -> Vec<ApprovalPreset> {
    vec![
        ApprovalPreset {
            id: "read-only",
            label: "Read Only",
            description: "Codex can read files in the current workspace. Approval is required to edit files or access the internet.",
            approval: AskForApproval::OnRequest,
            active_permission_profile: ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_READ_ONLY,
            ),
            permission_profile: PermissionProfile::read_only(),
        },
        ApprovalPreset {
            id: "auto",
            label: "Default",
            description: "Codex can read and edit files in the current workspace, and run commands. Approval is required to access the internet or edit other files. (Identical to Agent mode)",
            approval: AskForApproval::OnRequest,
            active_permission_profile: ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_WORKSPACE,
            ),
            permission_profile: PermissionProfile::workspace_write(),
        },
        ApprovalPreset {
            id: "full-access",
            label: "Full Access",
            description: "Codex can edit files outside this workspace and access the internet without asking for approval. Exercise caution when using.",
            approval: AskForApproval::Never,
            active_permission_profile: ActivePermissionProfile::new(
                BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS,
            ),
            permission_profile: PermissionProfile::Disabled,
        },
    ]
}

pub(crate) fn builtin_permission_profile_for_active_permission_profile(
    active_permission_profile: &ActivePermissionProfile,
) -> Option<PermissionProfile> {
    if active_permission_profile.extends.is_some() {
        return None;
    }
    match active_permission_profile.id.as_str() {
        BUILT_IN_PERMISSION_PROFILE_READ_ONLY => Some(PermissionProfile::read_only()),
        BUILT_IN_PERMISSION_PROFILE_WORKSPACE => Some(PermissionProfile::workspace_write()),
        BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS => Some(PermissionProfile::Disabled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_to_their_declared_profiles() {
        for preset in builtin_approval_presets() {
            assert_eq!(
                builtin_permission_profile_for_active_permission_profile(
                    &preset.active_permission_profile
                ),
                Some(preset.permission_profile)
            );
        }
    }
}
