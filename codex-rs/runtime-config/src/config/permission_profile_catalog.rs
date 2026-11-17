use codex_config::ConfigLayerStack;
use codex_config::permissions_toml::PermissionsToml;
use codex_protocol::models::PermissionProfile;

use super::merge_managed_permission_profiles;
use super::permissions::builtin_permission_profiles;
use super::permissions::compile_permission_profile_selection;
use super::permissions::validate_user_permission_profile_names;

/// A permission profile exposed to clients together with its effective availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionProfileCatalogEntry {
    pub id: String,
    pub description: Option<String>,
    pub allowed: bool,
}

/// Builds the effective permission profile catalog for a config layer stack.
pub fn permission_profile_catalog(
    config_layer_stack: &ConfigLayerStack,
) -> std::io::Result<Vec<PermissionProfileCatalogEntry>> {
    let permissions = config_layer_stack
        .effective_config()
        .get("permissions")
        .cloned()
        .map(toml::Value::try_into::<PermissionsToml>)
        .transpose()
        .map_err(std::io::Error::other)?;
    let requirements_toml = config_layer_stack.requirements_toml();
    let permissions = merge_managed_permission_profiles(permissions.as_ref(), requirements_toml)?;

    permission_profile_catalog_from_permissions(config_layer_stack, permissions.as_ref())
}

pub(super) fn permission_profile_catalog_from_permissions(
    config_layer_stack: &ConfigLayerStack,
    permissions: Option<&PermissionsToml>,
) -> std::io::Result<Vec<PermissionProfileCatalogEntry>> {
    validate_user_permission_profile_names(permissions)?;

    let mut catalog = builtin_permission_profiles()
        .map(|(id, permission_profile)| PermissionProfileCatalogEntry {
            id: id.to_string(),
            description: Some(
                "Full filesystem and network access; legacy sandbox restrictions are ignored."
                    .to_string(),
            ),
            allowed: permission_profile_is_allowed(config_layer_stack, id, &permission_profile),
        })
        .collect::<Vec<_>>();

    if let Some(permissions) = permissions {
        catalog.extend(permissions.entries.iter().map(|(id, profile)| {
            let mut warnings = Vec::new();
            let allowed = compile_permission_profile_selection(
                Some(permissions),
                id,
                /*workspace_write*/ None,
                &mut warnings,
            )
            .map(|(file_system, network)| {
                PermissionProfile::from_runtime_permissions(&file_system, network)
            })
            .is_ok_and(|permission_profile| {
                permission_profile_is_allowed(config_layer_stack, id, &permission_profile)
            });
            PermissionProfileCatalogEntry {
                id: id.clone(),
                description: profile.description.clone(),
                allowed,
            }
        }));
    }

    Ok(catalog)
}

pub(super) fn permission_profile_is_allowed(
    _config_layer_stack: &ConfigLayerStack,
    _profile_id: &str,
    _permission_profile: &PermissionProfile,
) -> bool {
    // Every accepted legacy selection resolves to the same full-access profile.
    true
}
