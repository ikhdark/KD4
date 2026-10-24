use std::collections::BTreeMap;

use crate::merge::merge_owned_toml_values;
use codex_protocol::permissions::FileSystemAccessMode;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use toml::Value as TomlValue;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(transparent)]
pub struct PermissionsToml {
    pub entries: BTreeMap<String, PermissionProfileToml>,
}

impl PermissionsToml {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve `profile_name` and all of its `extends` ancestors into one TOML
    /// profile.
    ///
    /// Parent profiles are merged before their children, so child keys override
    /// matching parent keys before callers compile the profile into runtime
    /// permissions. The returned profile keeps the selected profile's
    /// declaration metadata, such as `description` and `extends`.
    pub fn resolve_profile<F>(
        &self,
        profile_name: &str,
        mut parent_profile: F,
    ) -> Result<PermissionProfileToml, PermissionProfileResolutionError>
    where
        F: FnMut(&str) -> Option<PermissionProfileToml>,
    {
        let mut profile_names = Vec::new();
        let mut profiles = Vec::new();
        let mut next_profile_name = profile_name.to_string();
        let mut referenced_by: Option<String> = None;

        loop {
            if let Some(cycle_start) = profile_names
                .iter()
                .position(|name| name == &next_profile_name)
            {
                let cycle = profile_names[cycle_start..]
                    .iter()
                    .cloned()
                    .chain(std::iter::once(next_profile_name))
                    .collect::<Vec<_>>();
                return Err(PermissionProfileResolutionError::Cycle { cycle });
            }

            let profile = self
                .entries
                .get(&next_profile_name)
                .cloned()
                .or_else(|| parent_profile(&next_profile_name))
                .ok_or_else(|| {
                    referenced_by.as_deref().map_or_else(
                        || PermissionProfileResolutionError::UndefinedProfile {
                            profile_name: next_profile_name.clone(),
                        },
                        |referenced_by| {
                            if next_profile_name.starts_with(':') {
                                PermissionProfileResolutionError::UnsupportedBuiltInParent {
                                    profile_name: referenced_by.to_string(),
                                    parent_profile_name: next_profile_name.clone(),
                                }
                            } else {
                                PermissionProfileResolutionError::UndefinedParent {
                                    profile_name: referenced_by.to_string(),
                                    parent_profile_name: next_profile_name.clone(),
                                }
                            }
                        },
                    )
                })?;
            let parent_profile_name = profile.extends.clone();

            profile_names.push(next_profile_name.clone());

            if let Some(parent_profile_name) = parent_profile_name {
                profiles.push(profile);
                referenced_by = Some(next_profile_name);
                next_profile_name = parent_profile_name;
                continue;
            }

            let profile = profiles
                .into_iter()
                .rev()
                .try_fold(profile, merge_permission_profiles)?;
            return Ok(profile);
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PermissionProfileToml {
    pub description: Option<String>,
    pub extends: Option<String>,
    pub workspace_roots: Option<WorkspaceRootsToml>,
    pub filesystem: Option<FilesystemPermissionsToml>,
    pub network: Option<NetworkToml>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PermissionProfileResolutionError {
    #[error("default_permissions refers to undefined profile `{profile_name}`")]
    UndefinedProfile { profile_name: String },
    #[error(
        "permissions profile `{profile_name}` extends undefined profile `{parent_profile_name}`"
    )]
    UndefinedParent {
        profile_name: String,
        parent_profile_name: String,
    },
    #[error(
        "permissions profile `{profile_name}` cannot extend unsupported built-in profile `{parent_profile_name}`"
    )]
    UnsupportedBuiltInParent {
        profile_name: String,
        parent_profile_name: String,
    },
    #[error(
        "permissions profile inheritance cycle detected: {}",
        cycle.join(" -> ")
    )]
    Cycle { cycle: Vec<String> },
    #[error("failed to serialize permissions profile while resolving inheritance: {source}")]
    SerializeProfileToml {
        #[source]
        source: toml::ser::Error,
    },
    #[error(
        "failed to deserialize merged permissions profile while resolving inheritance: {source}"
    )]
    DeserializeProfileToml {
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid permissions profile `{profile_name}`: {reason}")]
    InvalidProfile { profile_name: String, reason: String },
}

fn merge_permission_profiles(
    mut parent: PermissionProfileToml,
    child: PermissionProfileToml,
) -> Result<PermissionProfileToml, PermissionProfileResolutionError> {
    // Description and inheritance metadata belong to the selected profile
    // declaration, so an inherited profile must not fill those gaps.
    parent.description = None;
    parent.extends = None;

    let mut merged = TomlValue::try_from(parent)
        .map_err(|source| PermissionProfileResolutionError::SerializeProfileToml { source })?;
    let child = TomlValue::try_from(child)
        .map_err(|source| PermissionProfileResolutionError::SerializeProfileToml { source })?;
    merge_owned_toml_values(&mut merged, child);
    merged
        .try_into()
        .map_err(|source| PermissionProfileResolutionError::DeserializeProfileToml { source })
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct WorkspaceRootsToml {
    #[serde(flatten)]
    pub entries: BTreeMap<String, bool>,
}

impl WorkspaceRootsToml {
    pub fn enabled_roots(&self) -> impl Iterator<Item = &String> {
        self.entries
            .iter()
            .filter_map(|(path, enabled)| (*enabled).then_some(path))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct FilesystemPermissionsToml {
    /// Optional maximum depth for expanding unreadable glob patterns on
    /// platforms that snapshot glob matches before sandbox startup.
    #[schemars(range(min = 1))]
    pub glob_scan_max_depth: Option<usize>,
    #[serde(flatten)]
    pub entries: BTreeMap<String, FilesystemPermissionToml>,
}

impl FilesystemPermissionsToml {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum FilesystemPermissionToml {
    Access(FileSystemAccessMode),
    Scoped(BTreeMap<String, FileSystemAccessMode>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct NetworkDomainPermissionsToml {
    #[serde(flatten)]
    pub entries: BTreeMap<String, NetworkDomainPermissionToml>,
}

impl NetworkDomainPermissionsToml {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn allowed_domains(&self) -> Option<Vec<String>> {
        let allowed_domains: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, permission)| matches!(permission, NetworkDomainPermissionToml::Allow))
            .map(|(pattern, _)| pattern.clone())
            .collect();
        (!allowed_domains.is_empty()).then_some(allowed_domains)
    }

    pub fn denied_domains(&self) -> Option<Vec<String>> {
        let denied_domains: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, permission)| matches!(permission, NetworkDomainPermissionToml::Deny))
            .map(|(pattern, _)| pattern.clone())
            .collect();
        (!denied_domains.is_empty()).then_some(denied_domains)
    }
}

#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDomainPermissionToml {
    Allow,
    Deny,
}

impl std::fmt::Display for NetworkDomainPermissionToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let permission = match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        };
        f.write_str(permission)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
pub struct NetworkUnixSocketPermissionsToml {
    #[serde(flatten)]
    pub entries: BTreeMap<String, NetworkUnixSocketPermissionToml>,
}

impl NetworkUnixSocketPermissionsToml {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn allow_unix_sockets(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, permission)| matches!(permission, NetworkUnixSocketPermissionToml::Allow))
            .map(|(path, _)| path.clone())
            .collect()
    }
}

#[derive(
    Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum NetworkUnixSocketPermissionToml {
    Allow,
    Deny,
}

impl std::fmt::Display for NetworkUnixSocketPermissionToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let permission = match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        };
        f.write_str(permission)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkToml {
    /// Allow network access inside the platform sandbox.
    pub enabled: Option<bool>,
}
