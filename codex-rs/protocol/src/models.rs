use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::num::NonZeroUsize;
use std::path::Path;

use codex_utils_image::PromptImageMode;
use codex_utils_image::data_url_from_bytes;
use codex_utils_image::load_for_prompt_bytes;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::ser::SerializeMap;
use serde::ser::Serializer;
use ts_rs::TS;

use crate::permissions::FileSystemAccessMode;
use crate::permissions::FileSystemPath;
use crate::permissions::FileSystemSandboxEntry;
use crate::permissions::FileSystemSandboxKind;
use crate::permissions::FileSystemSandboxPolicy;
use crate::permissions::FileSystemSpecialPath;
use crate::permissions::NetworkSandboxPolicy;
use crate::protocol::SandboxPolicy;
use crate::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_image::ImageProcessingError;
use codex_utils_path_uri::PathUri;
use codex_utils_string::approx_token_count;
use codex_utils_string::truncate_middle_with_token_budget;
use schemars::JsonSchema;

use crate::ResponseItemId;
use crate::mcp::CallToolResult;

/// Controls the per-command sandbox override requested by a shell-like tool call.
#[derive(
    Debug, Clone, Copy, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPermissions {
    /// Run with the turn's configured sandbox policy unchanged.
    #[default]
    UseDefault,
    /// Request to run outside the sandbox.
    RequireEscalated,
    /// Request to stay in the sandbox while widening permissions for this
    /// command only.
    WithAdditionalPermissions,
}

impl SandboxPermissions {
    /// True if SandboxPermissions requires full unsandboxed execution (i.e. RequireEscalated)
    pub fn requires_escalated_permissions(self) -> bool {
        matches!(self, SandboxPermissions::RequireEscalated)
    }

    /// True if SandboxPermissions requests any explicit per-command override
    /// beyond `UseDefault`.
    pub fn requests_sandbox_override(self) -> bool {
        !matches!(self, SandboxPermissions::UseDefault)
    }

    /// True if SandboxPermissions uses the sandboxed per-command permission
    /// widening flow.
    pub fn uses_additional_permissions(self) -> bool {
        matches!(self, SandboxPermissions::WithAdditionalPermissions)
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, TS)]
#[ts(as = "FileSystemPermissionsDe<PathType>")]
pub struct FileSystemPermissions<PathType = AbsolutePathBuf> {
    pub entries: Vec<FileSystemSandboxEntry<PathType>>,
    pub glob_scan_max_depth: Option<NonZeroUsize>,
}

impl<PathType: JsonSchema> JsonSchema for FileSystemPermissions<PathType> {
    fn schema_name() -> String {
        format!("FileSystemPermissions_for_{}", PathType::schema_name())
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        FileSystemPermissionsDe::<PathType>::json_schema(generator)
    }
}

impl From<FileSystemPermissions<AbsolutePathBuf>> for FileSystemPermissions<PathUri> {
    fn from(value: FileSystemPermissions<AbsolutePathBuf>) -> Self {
        FileSystemPermissions {
            entries: value
                .entries
                .into_iter()
                .map(FileSystemSandboxEntry::<PathUri>::from)
                .collect(),
            glob_scan_max_depth: value.glob_scan_max_depth,
        }
    }
}

impl TryFrom<FileSystemPermissions<PathUri>> for FileSystemPermissions<AbsolutePathBuf> {
    type Error = io::Error;

    fn try_from(value: FileSystemPermissions<PathUri>) -> Result<Self, Self::Error> {
        Ok(FileSystemPermissions {
            entries: value
                .entries
                .into_iter()
                .map(FileSystemSandboxEntry::<AbsolutePathBuf>::try_from)
                .collect::<io::Result<_>>()?,
            glob_scan_max_depth: value.glob_scan_max_depth,
        })
    }
}

impl<PathType> Default for FileSystemPermissions<PathType> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            glob_scan_max_depth: None,
        }
    }
}

pub type LegacyReadWriteRoots<PathType = AbsolutePathBuf> =
    (Option<Vec<PathType>>, Option<Vec<PathType>>);
impl<PathType> FileSystemPermissions<PathType> {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn from_read_write_roots(
        read: Option<Vec<PathType>>,
        write: Option<Vec<PathType>>,
    ) -> Self {
        let mut entries = Vec::new();
        if let Some(read) = read {
            entries.extend(read.into_iter().map(|path| FileSystemSandboxEntry {
                path: FileSystemPath::Path { path },
                access: FileSystemAccessMode::Read,
            }));
        }
        if let Some(write) = write {
            entries.extend(write.into_iter().map(|path| FileSystemSandboxEntry {
                path: FileSystemPath::Path { path },
                access: FileSystemAccessMode::Write,
            }));
        }
        Self {
            entries,
            glob_scan_max_depth: None,
        }
    }

    pub fn explicit_path_entries(&self) -> impl Iterator<Item = (&PathType, FileSystemAccessMode)> {
        self.entries.iter().filter_map(|entry| match &entry.path {
            FileSystemPath::Path { path } => Some((path, entry.access)),
            FileSystemPath::GlobPattern { .. } | FileSystemPath::Special { .. } => None,
        })
    }

    pub fn legacy_read_write_roots(&self) -> Option<LegacyReadWriteRoots<PathType>>
    where
        PathType: Clone,
    {
        self.as_legacy_permissions()
            .map(|legacy| (legacy.read, legacy.write))
    }

    fn as_legacy_permissions(&self) -> Option<LegacyFileSystemPermissions<PathType>>
    where
        PathType: Clone,
    {
        if self.glob_scan_max_depth.is_some() {
            return None;
        }

        let mut read = Vec::new();
        let mut write = Vec::new();

        for entry in &self.entries {
            let FileSystemPath::Path { path } = &entry.path else {
                return None;
            };
            match entry.access {
                FileSystemAccessMode::Read => read.push(path.clone()),
                FileSystemAccessMode::Write => write.push(path.clone()),
                FileSystemAccessMode::Deny => return None,
            }
        }

        Some(LegacyFileSystemPermissions {
            read: (!read.is_empty()).then_some(read),
            write: (!write.is_empty()).then_some(write),
        })
    }
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "PathType: Deserialize<'de>"))]
struct LegacyFileSystemPermissions<PathType = AbsolutePathBuf> {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    read: Option<Vec<PathType>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    write: Option<Vec<PathType>>,
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "PathType: Deserialize<'de>"))]
struct CanonicalFileSystemPermissions<PathType = AbsolutePathBuf> {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    entries: Vec<FileSystemSandboxEntry<PathType>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    glob_scan_max_depth: Option<NonZeroUsize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, TS)]
#[serde(untagged)]
enum FileSystemPermissionsDe<PathType = AbsolutePathBuf> {
    Canonical(#[ts(inline)] CanonicalFileSystemPermissions<PathType>),
    Legacy(#[ts(inline)] LegacyFileSystemPermissions<PathType>),
}

impl<PathType> Serialize for FileSystemPermissions<PathType>
where
    PathType: Clone + Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(legacy) = self.as_legacy_permissions() {
            legacy.serialize(serializer)
        } else {
            CanonicalFileSystemPermissions {
                entries: self.entries.clone(),
                glob_scan_max_depth: self.glob_scan_max_depth,
            }
            .serialize(serializer)
        }
    }
}

impl<'de, PathType> Deserialize<'de> for FileSystemPermissions<PathType>
where
    PathType: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match FileSystemPermissionsDe::deserialize(deserializer)? {
            FileSystemPermissionsDe::Canonical(CanonicalFileSystemPermissions {
                entries,
                glob_scan_max_depth,
            }) => Ok(Self {
                entries,
                glob_scan_max_depth,
            }),
            FileSystemPermissionsDe::Legacy(LegacyFileSystemPermissions { read, write }) => {
                Ok(Self::from_read_write_roots(read, write))
            }
        }
    }
}

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct NetworkPermissions {
    pub enabled: Option<bool>,
}

impl NetworkPermissions {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
    }
}

/// Partial permission overlay used for per-command requests and approved
/// session/turn grants.
#[derive(Debug, Clone, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct AdditionalPermissionProfile {
    pub network: Option<NetworkPermissions>,
    pub file_system: Option<FileSystemPermissions>,
}

impl AdditionalPermissionProfile {
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}

#[derive(
    Debug, Clone, Copy, Default, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    /// Codex owns sandbox construction for this profile.
    #[default]
    Managed,
    /// No outer filesystem sandbox should be applied.
    Disabled,
    /// Filesystem isolation is enforced by an external caller.
    External,
}

impl SandboxEnforcement {
    pub fn from_legacy_sandbox_policy(sandbox_policy: &SandboxPolicy) -> Self {
        match sandbox_policy {
            SandboxPolicy::DangerFullAccess => Self::Disabled,
            SandboxPolicy::ExternalSandbox { .. } => Self::External,
            SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. } => Self::Managed,
        }
    }
}

/// Filesystem permissions for profiles where Codex owns sandbox construction.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
pub enum ManagedFileSystemPermissions<PathType = AbsolutePathBuf> {
    /// Apply a managed filesystem sandbox from the listed entries.
    #[serde(rename_all = "snake_case")]
    #[ts(rename_all = "snake_case")]
    Restricted {
        entries: Vec<FileSystemSandboxEntry<PathType>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        glob_scan_max_depth: Option<NonZeroUsize>,
    },
    /// Apply a managed sandbox that allows all filesystem access.
    Unrestricted,
}

impl From<ManagedFileSystemPermissions<AbsolutePathBuf>> for ManagedFileSystemPermissions<PathUri> {
    fn from(value: ManagedFileSystemPermissions<AbsolutePathBuf>) -> Self {
        match value {
            ManagedFileSystemPermissions::Restricted {
                entries,
                glob_scan_max_depth,
            } => ManagedFileSystemPermissions::Restricted {
                entries: entries
                    .into_iter()
                    .map(FileSystemSandboxEntry::<PathUri>::from)
                    .collect(),
                glob_scan_max_depth,
            },
            ManagedFileSystemPermissions::Unrestricted => {
                ManagedFileSystemPermissions::Unrestricted
            }
        }
    }
}

impl TryFrom<ManagedFileSystemPermissions<PathUri>>
    for ManagedFileSystemPermissions<AbsolutePathBuf>
{
    type Error = io::Error;

    fn try_from(value: ManagedFileSystemPermissions<PathUri>) -> Result<Self, Self::Error> {
        Ok(match value {
            ManagedFileSystemPermissions::Restricted {
                entries,
                glob_scan_max_depth,
            } => ManagedFileSystemPermissions::Restricted {
                entries: entries
                    .into_iter()
                    .map(FileSystemSandboxEntry::<AbsolutePathBuf>::try_from)
                    .collect::<io::Result<_>>()?,
                glob_scan_max_depth,
            },
            ManagedFileSystemPermissions::Unrestricted => {
                ManagedFileSystemPermissions::Unrestricted
            }
        })
    }
}

impl ManagedFileSystemPermissions {
    fn from_sandbox_policy(file_system_sandbox_policy: &FileSystemSandboxPolicy) -> Self {
        match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::Restricted => Self::Restricted {
                entries: file_system_sandbox_policy.entries.clone(),
                glob_scan_max_depth: file_system_sandbox_policy
                    .glob_scan_max_depth
                    .and_then(NonZeroUsize::new),
            },
            FileSystemSandboxKind::Unrestricted => Self::Unrestricted,
            FileSystemSandboxKind::ExternalSandbox => unreachable!(
                "external filesystem policies are represented by PermissionProfile::External"
            ),
        }
    }

    pub fn to_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Restricted {
                entries,
                glob_scan_max_depth,
            } => FileSystemSandboxPolicy {
                kind: FileSystemSandboxKind::Restricted,
                glob_scan_max_depth: glob_scan_max_depth.map(usize::from),
                entries: entries.clone(),
            },
            Self::Unrestricted => FileSystemSandboxPolicy::unrestricted(),
        }
    }
}

/// Reserved identifier for the built-in read-only permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_READ_ONLY: &str = ":read-only";

/// Reserved identifier for the built-in workspace-write permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_WORKSPACE: &str = ":workspace";

/// Reserved identifier for the built-in full-access permission profile.
pub const BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS: &str = ":danger-full-access";

/// Canonical active runtime permissions for a conversation, turn, or command.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
pub enum PermissionProfile<PathType = AbsolutePathBuf> {
    /// Codex owns sandbox construction for this profile.
    #[serde(rename_all = "snake_case")]
    #[ts(rename_all = "snake_case")]
    Managed {
        file_system: ManagedFileSystemPermissions<PathType>,
        network: NetworkSandboxPolicy,
    },
    /// Do not apply an outer sandbox.
    Disabled,
    /// Filesystem isolation is enforced by an external caller.
    #[serde(rename_all = "snake_case")]
    #[ts(rename_all = "snake_case")]
    External { network: NetworkSandboxPolicy },
}

impl From<PermissionProfile<AbsolutePathBuf>> for PermissionProfile<PathUri> {
    fn from(value: PermissionProfile<AbsolutePathBuf>) -> Self {
        match value {
            PermissionProfile::Managed {
                file_system,
                network,
            } => PermissionProfile::Managed {
                file_system: file_system.into(),
                network,
            },
            PermissionProfile::Disabled => PermissionProfile::Disabled,
            PermissionProfile::External { network } => PermissionProfile::External { network },
        }
    }
}

impl TryFrom<PermissionProfile<PathUri>> for PermissionProfile<AbsolutePathBuf> {
    type Error = io::Error;

    fn try_from(value: PermissionProfile<PathUri>) -> Result<Self, Self::Error> {
        Ok(match value {
            PermissionProfile::Managed {
                file_system,
                network,
            } => PermissionProfile::Managed {
                file_system: file_system.try_into()?,
                network,
            },
            PermissionProfile::Disabled => PermissionProfile::Disabled,
            PermissionProfile::External { network } => PermissionProfile::External { network },
        })
    }
}

/// Metadata for the named or implicit built-in permissions profile that
/// produced the active `PermissionProfile`.
///
/// The runtime must honor `PermissionProfile`; this sidecar exists so clients
/// can display stable profile identity without trying to reverse-engineer a
/// name from the compiled permissions.
#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct ActivePermissionProfile {
    /// Profile identifier from `default_permissions` or the implicit built-in
    /// default, such as `:workspace` or a user-defined `[permissions.<id>]`
    /// profile.
    pub id: String,

    /// Optional parent profile identifier from the selected permissions
    /// profile's `extends` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub extends: Option<String>,
}

impl ActivePermissionProfile {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            extends: None,
        }
    }

    pub fn read_only() -> Self {
        Self::new(BUILT_IN_PERMISSION_PROFILE_READ_ONLY)
    }
}

impl<PathType> Default for PermissionProfile<PathType> {
    fn default() -> Self {
        Self::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: Vec::new(),
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        }
    }
}

impl PermissionProfile {
    /// Managed read-only filesystem access with restricted network access.
    pub fn read_only() -> Self {
        let file_system = FileSystemSandboxPolicy::read_only();
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network: NetworkSandboxPolicy::Restricted,
        }
    }

    /// Managed workspace-write filesystem access with restricted network
    /// access.
    ///
    /// The returned profile contains symbolic `:workspace_roots` entries that
    /// must be resolved against the active permission root before enforcement.
    pub fn workspace_write() -> Self {
        Self::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ false,
            /*exclude_slash_tmp*/ false,
        )
    }

    /// Managed workspace-write filesystem access with the legacy
    /// `sandbox_workspace_write` knobs applied directly to the profile.
    ///
    /// The returned profile contains symbolic `:workspace_roots` entries that
    /// must be resolved against the active permission root before enforcement.
    pub fn workspace_write_with(
        writable_roots: &[AbsolutePathBuf],
        network: NetworkSandboxPolicy,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let file_system = FileSystemSandboxPolicy::workspace_write(
            writable_roots,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        );
        Self::Managed {
            file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
            network,
        }
    }

    pub fn materialize_project_roots_with_workspace_roots(
        self,
        workspace_roots: &[AbsolutePathBuf],
    ) -> Self {
        match self {
            Self::Managed {
                file_system,
                network,
            } => {
                let file_system = file_system
                    .to_sandbox_policy()
                    .materialize_project_roots_with_workspace_roots(workspace_roots);
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(&file_system),
                    network,
                }
            }
            Self::Disabled => Self::Disabled,
            Self::External { network } => Self::External { network },
        }
    }

    pub fn from_runtime_permissions(
        file_system_sandbox_policy: &FileSystemSandboxPolicy,
        network_sandbox_policy: NetworkSandboxPolicy,
    ) -> Self {
        let enforcement = match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::Restricted | FileSystemSandboxKind::Unrestricted => {
                SandboxEnforcement::Managed
            }
            FileSystemSandboxKind::ExternalSandbox => SandboxEnforcement::External,
        };
        Self::from_runtime_permissions_with_enforcement(
            enforcement,
            file_system_sandbox_policy,
            network_sandbox_policy,
        )
    }

    pub fn from_runtime_permissions_with_enforcement(
        enforcement: SandboxEnforcement,
        file_system_sandbox_policy: &FileSystemSandboxPolicy,
        network_sandbox_policy: NetworkSandboxPolicy,
    ) -> Self {
        match file_system_sandbox_policy.kind {
            FileSystemSandboxKind::ExternalSandbox => Self::External {
                network: network_sandbox_policy,
            },
            FileSystemSandboxKind::Unrestricted if enforcement == SandboxEnforcement::Disabled => {
                Self::Disabled
            }
            FileSystemSandboxKind::Restricted | FileSystemSandboxKind::Unrestricted => {
                Self::Managed {
                    file_system: ManagedFileSystemPermissions::from_sandbox_policy(
                        file_system_sandbox_policy,
                    ),
                    network: network_sandbox_policy,
                }
            }
        }
    }

    pub fn from_legacy_sandbox_policy(sandbox_policy: &SandboxPolicy) -> Self {
        Self::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(sandbox_policy),
            &FileSystemSandboxPolicy::from(sandbox_policy),
            NetworkSandboxPolicy::from(sandbox_policy),
        )
    }

    pub fn from_legacy_sandbox_policy_for_cwd(sandbox_policy: &SandboxPolicy, cwd: &Path) -> Self {
        Self::from_runtime_permissions_with_enforcement(
            SandboxEnforcement::from_legacy_sandbox_policy(sandbox_policy),
            &FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(sandbox_policy, cwd),
            NetworkSandboxPolicy::from(sandbox_policy),
        )
    }

    pub fn enforcement(&self) -> SandboxEnforcement {
        match self {
            Self::Managed { .. } => SandboxEnforcement::Managed,
            Self::Disabled => SandboxEnforcement::Disabled,
            Self::External { .. } => SandboxEnforcement::External,
        }
    }

    pub fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Managed { file_system, .. } => file_system.to_sandbox_policy(),
            Self::Disabled => FileSystemSandboxPolicy::unrestricted(),
            Self::External { .. } => FileSystemSandboxPolicy::external_sandbox(),
        }
    }

    pub fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        match self {
            Self::Managed { network, .. } | Self::External { network } => *network,
            Self::Disabled => NetworkSandboxPolicy::Enabled,
        }
    }

    pub fn to_legacy_sandbox_policy(&self, cwd: &Path) -> io::Result<SandboxPolicy> {
        match self {
            Self::Managed {
                file_system,
                network,
            } => file_system
                .to_sandbox_policy()
                .to_legacy_sandbox_policy(*network, cwd),
            Self::Disabled => Ok(SandboxPolicy::DangerFullAccess),
            Self::External { network } => Ok(SandboxPolicy::ExternalSandbox {
                network_access: if network.is_enabled() {
                    crate::protocol::NetworkAccess::Enabled
                } else {
                    crate::protocol::NetworkAccess::Restricted
                },
            }),
        }
    }

    pub fn to_runtime_permissions(&self) -> (FileSystemSandboxPolicy, NetworkSandboxPolicy) {
        (
            self.file_system_sandbox_policy(),
            self.network_sandbox_policy(),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaggedPermissionProfile<PathType = AbsolutePathBuf> {
    #[serde(rename_all = "snake_case")]
    Managed {
        file_system: ManagedFileSystemPermissions<PathType>,
        network: NetworkSandboxPolicy,
    },
    Disabled,
    #[serde(rename_all = "snake_case")]
    External {
        network: NetworkSandboxPolicy,
    },
}

impl<PathType> From<TaggedPermissionProfile<PathType>> for PermissionProfile<PathType> {
    fn from(value: TaggedPermissionProfile<PathType>) -> Self {
        match value {
            TaggedPermissionProfile::Managed {
                file_system,
                network,
            } => Self::Managed {
                file_system,
                network,
            },
            TaggedPermissionProfile::Disabled => Self::Disabled,
            TaggedPermissionProfile::External { network } => Self::External { network },
        }
    }
}

/// Pre-tagged shape written to rollout files before `PermissionProfile`
/// represented enforcement explicitly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPermissionProfile<PathType = AbsolutePathBuf> {
    network: Option<NetworkPermissions>,
    file_system: Option<FileSystemPermissions<PathType>>,
}

impl<PathType> From<LegacyPermissionProfile<PathType>> for PermissionProfile<PathType> {
    fn from(value: LegacyPermissionProfile<PathType>) -> Self {
        let file_system = value.file_system.map_or_else(
            || ManagedFileSystemPermissions::Restricted {
                entries: Vec::new(),
                glob_scan_max_depth: None,
            },
            |permissions| ManagedFileSystemPermissions::Restricted {
                entries: permissions.entries,
                glob_scan_max_depth: permissions.glob_scan_max_depth,
            },
        );
        let network_sandbox_policy = if value
            .network
            .as_ref()
            .and_then(|network| network.enabled)
            .unwrap_or(false)
        {
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        };
        Self::Managed {
            file_system,
            network: network_sandbox_policy,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PermissionProfileDe<PathType = AbsolutePathBuf> {
    Tagged(TaggedPermissionProfile<PathType>),
    Legacy(LegacyPermissionProfile<PathType>),
}

impl<'de, PathType> Deserialize<'de> for PermissionProfile<PathType>
where
    PathType: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match PermissionProfileDe::deserialize(deserializer)? {
            PermissionProfileDe::Tagged(tagged) => tagged.into(),
            PermissionProfileDe::Legacy(legacy) => legacy.into(),
        })
    }
}

impl From<NetworkSandboxPolicy> for NetworkPermissions {
    fn from(value: NetworkSandboxPolicy) -> Self {
        Self {
            enabled: Some(value.is_enabled()),
        }
    }
}

impl From<&FileSystemSandboxPolicy> for FileSystemPermissions {
    fn from(value: &FileSystemSandboxPolicy) -> Self {
        let entries = match value.kind {
            FileSystemSandboxKind::Restricted => value.entries.clone(),
            FileSystemSandboxKind::Unrestricted | FileSystemSandboxKind::ExternalSandbox => {
                vec![FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Write,
                }]
            }
        };
        Self {
            entries,
            glob_scan_max_depth: value.glob_scan_max_depth.and_then(NonZeroUsize::new),
        }
    }
}

impl From<&FileSystemPermissions> for FileSystemSandboxPolicy {
    fn from(value: &FileSystemPermissions) -> Self {
        let mut policy = FileSystemSandboxPolicy::restricted(value.entries.clone());
        policy.glob_scan_max_depth = value.glob_scan_max_depth.map(usize::from);
        policy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputItem {
    Message {
        role: String,
        content: Vec<ContentItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        phase: Option<MessagePhase>,
    },
    FunctionCallOutput {
        call_id: String,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
    },
    McpToolCallOutput {
        call_id: String,
        output: CallToolResult,
    },
    CustomToolCallOutput {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
    },
    ToolSearchOutput {
        call_id: String,
        status: String,
        execution: String,
        #[ts(type = "unknown[]")]
        tools: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        omitted_result_count: Option<usize>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },
    OutputText {
        text: String,
    },
    /// Provider refusal content, retained as a distinct type in history and replay.
    Refusal {
        refusal: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageInputContent {
    InputText { text: String },
    EncryptedContent { encrypted_content: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

/// Image input supports embedded bytes only; this classifier never fetches URLs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageUrlKind {
    InlineData,
    Remote,
    Unsupported,
}

pub fn classify_image_url(url: &str) -> ImageUrlKind {
    if url
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        ImageUrlKind::InlineData
    } else if url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    }) {
        ImageUrlKind::Remote
    } else {
        ImageUrlKind::Unsupported
    }
}

pub const DEFAULT_IMAGE_DETAIL: ImageDetail = ImageDetail::High;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
/// Classifies assistant text as commentary, a partial answer, or a terminal answer.
///
/// Providers do not emit this consistently, so callers must treat `None` as
/// "phase unknown" and keep compatibility behavior for legacy models.
pub enum MessagePhase {
    /// Mid-turn assistant text (for example preamble/progress narration).
    ///
    /// Additional tool calls or assistant output may follow before turn
    /// completion.
    Commentary,
    /// Stable answer text that may be followed by more assistant output or tools.
    PartialAnswer,
    /// The assistant's terminal answer text for the current turn.
    FinalAnswer,
}

/// Internal Responses API passthrough metadata copied into underlying chat messages.
///
/// Responses API strongly types this payload. Do not modify it without first getting API
/// approval and making the corresponding Responses API change.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
pub struct InternalChatMessageMetadataPassthrough {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
}

impl InternalChatMessageMetadataPassthrough {
    pub(crate) fn set_turn_id_if_missing(metadata: &mut Option<Self>, turn_id: &str) {
        if turn_id.is_empty()
            || metadata
                .as_ref()
                .and_then(|metadata| metadata.turn_id.as_deref())
                .is_some_and(|turn_id| !turn_id.is_empty())
        {
            return;
        }
        metadata.get_or_insert_with(Self::default).turn_id = Some(turn_id.to_string());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    #[schemars(skip)]
    #[ts(skip)]
    AdditionalTools {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ResponseItemId>,
        role: String,
        tools: Vec<serde_json::Value>,
    },
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        role: String,
        content: Vec<ContentItem>,
        // Optional output-message phase (for example: "commentary", "final_answer").
        // Availability varies by provider/model, so downstream consumers must
        // preserve fallback behavior when this is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        phase: Option<MessagePhase>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    AgentMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        author: String,
        recipient: String,
        content: Vec<AgentMessageInputContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        summary: Vec<ReasoningItemReasoningSummary>,
        #[serde(default, skip_serializing_if = "should_serialize_reasoning_content")]
        #[ts(optional)]
        content: Option<Vec<ReasoningItemContent>>,
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    LocalShellCall {
        /// Legacy id field retained for compatibility with older payloads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        /// Set when using the Responses API.
        call_id: Option<String>,
        status: LocalShellStatus,
        action: LocalShellAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    FunctionCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        namespace: Option<String>,
        // The Responses API returns the function call arguments as a *string* that contains
        // JSON, not as an already‑parsed object. We keep it as a raw string here and let
        // Session::handle_function_call parse it into a Value.
        arguments: String,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    ToolSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        execution: String,
        #[ts(type = "unknown")]
        arguments: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // NOTE: The `output` field for `function_call_output` uses a dedicated payload type with
    // custom serialization. On the wire it is either:
    //   - a plain string (`content`)
    //   - an array of structured content items (`content_items`)
    // We keep this behavior centralized in `FunctionCallOutputPayload`.
    FunctionCallOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: String,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    CustomToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,

        call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        namespace: Option<String>,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // `custom_tool_call_output.output` uses the same wire encoding as
    // `function_call_output.output` so freeform tools can return either plain
    // text or structured content items.
    CustomToolCallOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    ToolSearchOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: Option<String>,
        status: String,
        execution: String,
        #[ts(type = "unknown[]")]
        tools: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        omitted_result_count: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // Emitted by the Responses API when the agent triggers a web search.
    // Example payload (from SSE `response.output_item.done`):
    // {
    //   "id":"ws_...",
    //   "type":"web_search_call",
    //   "status":"completed",
    //   "action": {"type":"search","query":"weather: San Francisco, CA"}
    // }
    WebSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        action: Option<WebSearchAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // Emitted by the Responses API when the agent triggers image generation.
    // Example payload:
    // {
    //   "id":"ig_123",
    //   "type":"image_generation_call",
    //   "status":"completed",
    //   "revised_prompt":"A gray tabby cat hugging an otter...",
    //   "result":"..."
    // }
    ImageGenerationCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        revised_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    #[serde(alias = "compaction_summary")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        encrypted_content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // Compaction triggers are request controls, not durable response items.
    CompactionTrigger {},
    ContextCompaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    #[serde(other)]
    Other,
}

impl ResponseItem {
    /// Returns whether this item is an ordinary user-role message.
    pub fn is_user_message(&self) -> bool {
        matches!(self, Self::Message { role, .. } if role == "user")
    }

    /// Returns the Responses API item ID, if present.
    pub fn id(&self) -> Option<&ResponseItemId> {
        match self {
            Self::AdditionalTools { id, .. }
            | Self::Message { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::LocalShellCall { id, .. }
            | Self::FunctionCall { id, .. }
            | Self::ToolSearchCall { id, .. }
            | Self::FunctionCallOutput { id, .. }
            | Self::CustomToolCall { id, .. }
            | Self::CustomToolCallOutput { id, .. }
            | Self::ToolSearchOutput { id, .. }
            | Self::WebSearchCall { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ImageGenerationCall { id, .. }
            | Self::Compaction { id, .. }
            | Self::ContextCompaction { id, .. } => id.as_ref(),
            Self::CompactionTrigger { .. } | Self::Other => None,
        }
    }

    /// Sets or clears the Responses API item ID for variants that carry one.
    pub fn set_id(&mut self, new_id: Option<ResponseItemId>) {
        match self {
            Self::AdditionalTools { id, .. }
            | Self::Message { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::LocalShellCall { id, .. }
            | Self::FunctionCall { id, .. }
            | Self::ToolSearchCall { id, .. }
            | Self::FunctionCallOutput { id, .. }
            | Self::CustomToolCall { id, .. }
            | Self::CustomToolCallOutput { id, .. }
            | Self::ToolSearchOutput { id, .. }
            | Self::WebSearchCall { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ImageGenerationCall { id, .. }
            | Self::Compaction { id, .. }
            | Self::ContextCompaction { id, .. } => *id = new_id,
            Self::CompactionTrigger { .. } | Self::Other => {}
        }
    }

    /// Returns the Responses API item ID prefix for variants that carry an ID.
    pub fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::AdditionalTools { .. } => Some("at"),
            Self::Message { .. } => Some("msg"),
            Self::AgentMessage { .. } => Some("amsg"),
            Self::Reasoning { .. } => Some("rs"),
            Self::LocalShellCall { .. } => Some("lsh"),
            Self::FunctionCall { .. } => Some("fc"),
            Self::ToolSearchCall { .. } => Some("tsc"),
            Self::FunctionCallOutput { .. } => Some("fco"),
            Self::CustomToolCall { .. } => Some("ctc"),
            Self::CustomToolCallOutput { .. } => Some("ctco"),
            Self::ToolSearchOutput { .. } => Some("tso"),
            Self::WebSearchCall { .. } => Some("ws"),
            Self::ImageGenerationCall { .. } => Some("ig"),
            Self::Compaction { .. } | Self::ContextCompaction { .. } => Some("cmp"),
            Self::CompactionTrigger { .. } | Self::Other => None,
        }
    }

    /// Returns the non-empty turn ID stamped onto this item, if present.
    pub fn turn_id(&self) -> Option<&str> {
        self.internal_chat_message_metadata_passthrough()
            .and_then(|metadata| metadata.turn_id.as_deref())
            .filter(|turn_id| !turn_id.is_empty())
    }

    /// Stamps the item with `turn_id` unless it already has a non-empty turn ID.
    pub fn set_turn_id_if_missing(&mut self, turn_id: &str) {
        let Some(metadata) = self.internal_chat_message_metadata_passthrough_mut() else {
            return;
        };
        InternalChatMessageMetadataPassthrough::set_turn_id_if_missing(metadata, turn_id);
    }

    /// Removes internal chat message metadata passthrough before sending to a provider that does
    /// not accept it.
    pub fn clear_internal_chat_message_metadata_passthrough(&mut self) {
        if let Some(metadata) = self.internal_chat_message_metadata_passthrough_mut() {
            *metadata = None;
        }
    }

    /// Serialize the same JSON bytes as a metadata-cleared clone, borrowing the
    /// model-visible fields. Keep field order and omission rules aligned with
    /// `ResponseItem` because prepared-history fingerprints include those bytes.
    pub fn serialize_without_internal_chat_message_metadata_passthrough<S>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.internal_chat_message_metadata_passthrough().is_none() {
            return self.serialize(serializer);
        }
        macro_rules! serialize_item {
            ($tag:literal, $($field:ident $(if $include:expr)?),* $(,)?) => {{
                let mut item = serializer.serialize_map(None)?;
                item.serialize_entry("type", $tag)?;
                $(
                    if true $(&& ($include))? {
                        item.serialize_entry(stringify!($field), $field)?;
                    }
                )*
                item.end()
            }};
        }
        // Explicit field patterns make additions to metadata-bearing variants
        // require updating this normalized representation as well.
        match self {
            Self::Message { id, role, content, phase,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "message", id if id.is_some(), role, content, phase if phase.is_some(),
            ),
            Self::AgentMessage { id, author, recipient, content,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "agent_message", id if id.is_some(), author, recipient, content,
            ),
            Self::Reasoning { id, summary, content, encrypted_content,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "reasoning", id if id.is_some(), summary,
                content if !should_serialize_reasoning_content(content), encrypted_content,
            ),
            Self::LocalShellCall { id, call_id, status, action,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "local_shell_call", id if id.is_some(), call_id, status, action,
            ),
            Self::FunctionCall { id, name, namespace, arguments, call_id,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "function_call", id if id.is_some(), name, namespace if namespace.is_some(),
                arguments, call_id,
            ),
            Self::ToolSearchCall { id, call_id, status, execution, arguments,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "tool_search_call", id if id.is_some(), call_id,
                status if status.is_some(), execution, arguments,
            ),
            Self::FunctionCallOutput { id, call_id, output,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "function_call_output", id if id.is_some(), call_id, output,
            ),
            Self::CustomToolCall { id, status, call_id, name, namespace, input,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "custom_tool_call", id if id.is_some(), status if status.is_some(),
                call_id, name, namespace if namespace.is_some(), input,
            ),
            Self::CustomToolCallOutput { id, call_id, name, output,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "custom_tool_call_output", id if id.is_some(), call_id,
                name if name.is_some(), output,
            ),
            Self::ToolSearchOutput {
                id, call_id, status, execution, tools, omitted_result_count,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "tool_search_output", id if id.is_some(), call_id, status,
                execution, tools, omitted_result_count if omitted_result_count.is_some(),
            ),
            Self::WebSearchCall { id, status, action,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "web_search_call", id if id.is_some(), status if status.is_some(),
                action if action.is_some(),
            ),
            Self::ImageGenerationCall { id, status, revised_prompt, result, error,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "image_generation_call", id if id.is_some(), status,
                revised_prompt if revised_prompt.is_some(), result if result.is_some(),
                error if error.is_some(),
            ),
            Self::Compaction { id, encrypted_content,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "compaction", id if id.is_some(), encrypted_content,
            ),
            Self::ContextCompaction { id, encrypted_content,
                internal_chat_message_metadata_passthrough: _,
            } => serialize_item!(
                "context_compaction", id if id.is_some(),
                encrypted_content if encrypted_content.is_some(),
            ),
            Self::AdditionalTools { .. } | Self::CompactionTrigger { .. } | Self::Other => {
                self.serialize(serializer)
            }
        }
    }

    pub fn internal_chat_message_metadata_passthrough(
        &self,
    ) -> Option<&InternalChatMessageMetadataPassthrough> {
        match self {
            Self::Message {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::AgentMessage {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Reasoning {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::LocalShellCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::WebSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ImageGenerationCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Compaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ContextCompaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => metadata.as_ref(),
            Self::CompactionTrigger { .. } | Self::AdditionalTools { .. } | Self::Other => None,
        }
    }

    fn internal_chat_message_metadata_passthrough_mut(
        &mut self,
    ) -> Option<&mut Option<InternalChatMessageMetadataPassthrough>> {
        match self {
            Self::Message {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::AgentMessage {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Reasoning {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::LocalShellCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::WebSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ImageGenerationCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Compaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ContextCompaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => Some(metadata),
            Self::CompactionTrigger { .. } | Self::AdditionalTools { .. } | Self::Other => None,
        }
    }
}

pub const BASE_INSTRUCTIONS_DEFAULT: &str = include_str!("prompts/base_instructions/default.md");

/// Base instructions for the model in a thread. Corresponds to the `instructions` field in the ResponsesAPI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename = "base_instructions", rename_all = "snake_case")]
pub struct BaseInstructions {
    pub text: String,
}

impl Default for BaseInstructions {
    fn default() -> Self {
        Self {
            text: BASE_INSTRUCTIONS_DEFAULT.trim().to_string(),
        }
    }
}

const MAX_RENDERED_PREFIXES: usize = 100;
const MAX_ALLOW_PREFIX_TEXT_BYTES: usize = 5000;
const TRUNCATED_MARKER: &str = "...\n[Some commands were truncated]";

pub fn format_allow_prefixes(prefixes: Vec<Vec<String>>) -> Option<String> {
    let mut truncated = false;
    if prefixes.len() > MAX_RENDERED_PREFIXES {
        truncated = true;
    }

    let mut prefixes = prefixes;
    prefixes.sort_by(|a, b| {
        a.len()
            .cmp(&b.len())
            .then_with(|| prefix_combined_str_len(a).cmp(&prefix_combined_str_len(b)))
            .then_with(|| a.cmp(b))
    });

    let full_text = prefixes
        .into_iter()
        .take(MAX_RENDERED_PREFIXES)
        .map(|prefix| format!("- {}", render_command_prefix(&prefix)))
        .collect::<Vec<_>>()
        .join("\n");

    let mut output = full_text;
    if output.len() > MAX_ALLOW_PREFIX_TEXT_BYTES {
        let mut end = MAX_ALLOW_PREFIX_TEXT_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        truncated = true;
    }

    if truncated {
        Some(format!("{output}{TRUNCATED_MARKER}"))
    } else {
        Some(output)
    }
}

fn prefix_combined_str_len(prefix: &[String]) -> usize {
    prefix.iter().map(String::len).sum()
}

fn render_command_prefix(prefix: &[String]) -> String {
    let tokens = prefix
        .iter()
        .map(|token| serde_json::to_string(token).unwrap_or_else(|_| format!("{token:?}")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{tokens}]")
}

fn should_serialize_reasoning_content(content: &Option<Vec<ReasoningItemContent>>) -> bool {
    match content {
        Some(content) => !content
            .iter()
            .any(|c| matches!(c, ReasoningItemContent::ReasoningText { .. })),
        None => false,
    }
}

fn local_image_error_placeholder(
    path: &std::path::Path,
    error: impl std::fmt::Display,
) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Codex could not read the local image at `{}`: {}",
            path.display(),
            error
        ),
    }
}

pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";

const IMAGE_OPEN_TAG: &str = "<image>";
const IMAGE_CLOSE_TAG: &str = "</image>";
const LOCAL_IMAGE_OPEN_TAG_PREFIX: &str = "<image name=";
const LOCAL_IMAGE_OPEN_TAG_SUFFIX: &str = ">";
const LOCAL_IMAGE_CLOSE_TAG: &str = IMAGE_CLOSE_TAG;

pub fn image_open_tag_text() -> String {
    IMAGE_OPEN_TAG.to_string()
}

pub fn image_close_tag_text() -> String {
    IMAGE_CLOSE_TAG.to_string()
}

pub fn local_image_label_text(label_number: usize) -> String {
    format!("[Image #{label_number}]")
}

pub fn local_image_open_tag_text_with_path(label_number: usize, path: &std::path::Path) -> String {
    let label = local_image_label_text(label_number);
    let path = path.to_string_lossy();
    let path = codex_utils_string::xml_attribute(&path);
    format!("{LOCAL_IMAGE_OPEN_TAG_PREFIX}{label} path=\"{path}\"{LOCAL_IMAGE_OPEN_TAG_SUFFIX}")
}

pub fn is_local_image_open_tag_text(text: &str) -> bool {
    text.strip_prefix(LOCAL_IMAGE_OPEN_TAG_PREFIX)
        .is_some_and(|rest| rest.ends_with(LOCAL_IMAGE_OPEN_TAG_SUFFIX))
}

pub fn is_local_image_close_tag_text(text: &str) -> bool {
    is_image_close_tag_text(text)
}

pub fn is_image_open_tag_text(text: &str) -> bool {
    text == IMAGE_OPEN_TAG
}

pub fn is_image_close_tag_text(text: &str) -> bool {
    text == IMAGE_CLOSE_TAG
}

fn invalid_image_error_placeholder(
    path: &std::path::Path,
    error: impl std::fmt::Display,
) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Image located at `{}` is invalid: {}",
            path.display(),
            error
        ),
    }
}

fn unsupported_image_error_placeholder(path: &std::path::Path, mime: &str) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Codex cannot attach image at `{}`: unsupported image `{}`.",
            path.display(),
            mime
        ),
    }
}

pub fn local_image_content_items_with_label_number(
    path: &std::path::Path,
    file_bytes: Vec<u8>,
    label_number: Option<usize>,
    detail: ImageDetail,
) -> Vec<ContentItem> {
    let mode = match detail {
        ImageDetail::Original => PromptImageMode::Original,
        ImageDetail::Auto | ImageDetail::Low | ImageDetail::High => PromptImageMode::ResizeToFit,
    };

    match load_for_prompt_bytes(path, file_bytes, mode) {
        Ok(image) => local_image_content_items(path, image.into_data_url(), label_number, detail),
        Err(err) => match &err {
            ImageProcessingError::Read { .. }
            | ImageProcessingError::Encode { .. }
            | ImageProcessingError::InvalidDataUrl { .. }
            | ImageProcessingError::InvalidResizeLimits
            | ImageProcessingError::ImageTooLarge { .. } => {
                vec![local_image_error_placeholder(path, &err)]
            }
            ImageProcessingError::Decode { .. } if err.is_invalid_image() => {
                vec![invalid_image_error_placeholder(path, &err)]
            }
            ImageProcessingError::Decode { .. } => {
                vec![local_image_error_placeholder(path, &err)]
            }
            ImageProcessingError::UnsupportedImageFormat { mime } => {
                vec![unsupported_image_error_placeholder(path, mime)]
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalImagePreparation {
    Process,
    Defer,
}

/// Formats an acquired snapshot without reopening the path. Shared by sync
/// conversion and the session's sequential, cancellable acquisition owner.
pub fn local_image_content_items_from_acquired_bytes(
    path: &Path,
    bytes: std::io::Result<Vec<u8>>,
    image_index: usize,
    detail: ImageDetail,
    local_image_preparation: LocalImagePreparation,
) -> Vec<ContentItem> {
    match bytes {
        Ok(file_bytes) if file_bytes.len() > codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES => {
            vec![local_image_error_placeholder(
                path,
                "image source exceeds the 64 MiB limit",
            )]
        }
        Ok(file_bytes) => match local_image_preparation {
            LocalImagePreparation::Process => local_image_content_items_with_label_number(
                path,
                file_bytes,
                Some(image_index),
                detail,
            ),
            LocalImagePreparation::Defer => local_image_content_items(
                path,
                data_url_from_bytes("application/octet-stream", &file_bytes),
                Some(image_index),
                detail,
            ),
        },
        Err(err) => vec![local_image_error_placeholder(path, err)],
    }
}

fn local_image_content_items(
    path: &std::path::Path,
    image_url: String,
    label_number: Option<usize>,
    detail: ImageDetail,
) -> Vec<ContentItem> {
    let mut items = Vec::with_capacity(3);
    if let Some(label_number) = label_number {
        items.push(ContentItem::InputText {
            text: local_image_open_tag_text_with_path(label_number, path),
        });
    }
    items.push(ContentItem::InputImage {
        image_url,
        detail: Some(detail),
    });
    if label_number.is_some() {
        items.push(ContentItem::InputText {
            text: LOCAL_IMAGE_CLOSE_TAG.to_string(),
        });
    }
    items
}

impl From<ResponseInputItem> for ResponseItem {
    fn from(item: ResponseInputItem) -> Self {
        match item {
            ResponseInputItem::Message {
                role,
                content,
                phase,
            } => Self::Message {
                role,
                content,
                id: None,
                phase,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::FunctionCallOutput { call_id, output } => Self::FunctionCallOutput {
                id: None,
                call_id,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::McpToolCallOutput { call_id, output } => {
                let output = output.into_function_call_output_payload();
                Self::FunctionCallOutput {
                    id: None,
                    call_id,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                }
            }
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                name,
                output,
            } => Self::CustomToolCallOutput {
                id: None,
                call_id,
                name,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::ToolSearchOutput {
                call_id,
                status,
                execution,
                tools,
                omitted_result_count,
            } => Self::ToolSearchOutput {
                call_id: Some(call_id),
                status,
                execution,
                tools,
                omitted_result_count,
                id: None,
                internal_chat_message_metadata_passthrough: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum LocalShellStatus {
    Completed,
    InProgress,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalShellAction {
    Exec(LocalShellExecAction),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
pub struct LocalShellExecAction {
    pub command: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub working_directory: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(rename = "ResponsesApiWebSearchAction")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        pattern: Option<String>,
    },

    #[serde(other)]
    Other,
}

fn web_search_query_detail(query: &Option<String>, queries: &Option<Vec<String>>) -> String {
    query
        .clone()
        .filter(|query| !query.is_empty())
        .unwrap_or_else(|| {
            let items = queries.as_ref();
            let first = items
                .and_then(|queries| queries.first())
                .cloned()
                .unwrap_or_default();
            if items.is_some_and(|queries| queries.len() > 1) && !first.is_empty() {
                format!("{first} ...")
            } else {
                first
            }
        })
}

pub fn web_search_action_detail(action: &WebSearchAction) -> String {
    match action {
        WebSearchAction::Search { query, queries } => web_search_query_detail(query, queries),
        WebSearchAction::OpenPage { url } => url.clone().unwrap_or_default(),
        WebSearchAction::FindInPage { url, pattern } => match (pattern, url) {
            (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
            (Some(pattern), None) => format!("'{pattern}'"),
            (None, Some(url)) => url.clone(),
            (None, None) => String::new(),
        },
        WebSearchAction::Other => String::new(),
    }
}

pub fn web_search_detail(action: Option<&WebSearchAction>, query: &str) -> String {
    let detail = action.map(web_search_action_detail).unwrap_or_default();
    if detail.is_empty() {
        query.to_string()
    } else {
        detail
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningItemReasoningSummary {
    SummaryText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningItemContent {
    ReasoningText { text: String },
    Text { text: String },
}

impl From<Vec<UserInput>> for ResponseInputItem {
    fn from(items: Vec<UserInput>) -> Self {
        Self::from_user_input(items, LocalImagePreparation::Process)
    }
}

// Shared across selected paths in one user message, including their envelopes.
const LOCAL_PATH_CONTEXT_TOKEN_BUDGET: usize = 10_000;
const LOCAL_PATH_CONTEXT_RETRY_AVOIDANCE_TOKEN_MARGIN: usize = 128;
const LOCAL_PATH_CONTEXT_METADATA_OMISSION: &str = "<local_path_context_omission recovery=\"Select fewer paths or shorter path names; do not infer missing content\" />";

fn render_local_path_context(
    path: &Path,
    content: &str,
    token_budget: usize,
    retry_margin: usize,
) -> String {
    // Escape the attribute delimiter, not native path separators as Rust Debug does.
    let path = path.to_string_lossy();
    let path = codex_utils_string::xml_attribute(&path);
    let prefix = format!("<local_path_context path=\"{path}\">\n");
    let suffix = "\n</local_path_context>";
    let envelope_tokens = approx_token_count(&prefix) + approx_token_count(suffix);
    let encoded = codex_utils_string::xml_text(content).to_string();
    let content_tokens = approx_token_count(&encoded);
    if envelope_tokens.saturating_add(content_tokens) <= token_budget.saturating_add(retry_margin) {
        return format!("{prefix}{encoded}{suffix}");
    }

    let omission = format!(
        "\n<local_path_context_omission estimated_original_content_tokens={content_tokens} \
         recovery=\"read the original local path above; do not infer missing content\" />"
    );
    let metadata_tokens = envelope_tokens + approx_token_count(&omission);
    if metadata_tokens > token_budget {
        return LOCAL_PATH_CONTEXT_METADATA_OMISSION.to_string();
    }
    // These pieces are separated by whitespace, so the sum of their estimates
    // bounds the joined estimate. The truncator already includes its own marker.
    // Clip source, then encode. Cutting an encoded entity would lose the exact
    // meaning of the delivered prefix/tail, and pre-escape budgets can overrun.
    let mut low = 0;
    let mut high = token_budget - metadata_tokens;
    let mut bounded_content = String::new();
    while low < high {
        let budget = low + (high - low).div_ceil(2);
        let candidate = truncate_middle_with_token_budget(content, budget).0;
        let candidate = codex_utils_string::xml_text(&candidate).to_string();
        if approx_token_count(&candidate) + metadata_tokens <= token_budget {
            low = budget;
            bounded_content = candidate;
        } else {
            high = budget - 1;
        }
    }
    format!("{prefix}{bounded_content}{omission}{suffix}")
}

impl ResponseInputItem {
    pub fn from_user_input(
        items: Vec<UserInput>,
        local_image_preparation: LocalImagePreparation,
    ) -> Self {
        Self::from_user_input_with_local_images(
            items,
            local_image_preparation,
            codex_utils_image::read_image_source,
        )
    }

    pub fn from_user_input_with_local_images(
        items: Vec<UserInput>,
        local_image_preparation: LocalImagePreparation,
        mut read_image: impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
    ) -> Self {
        Self::from_user_input_with_local_image_content(items, |path, index, detail| {
            local_image_content_items_from_acquired_bytes(
                path,
                read_image(path),
                index,
                detail,
                local_image_preparation,
            )
        })
    }

    /// Conversion after acquisition lets async owners supply bounded snapshots
    /// without reading local files synchronously on the session worker.
    pub fn from_user_input_with_local_image_content(
        items: Vec<UserInput>,
        mut local_image: impl FnMut(&Path, usize, ImageDetail) -> Vec<ContentItem>,
    ) -> Self {
        let mut image_index = 0;
        let path_demands: Vec<usize> = items
            .iter()
            .filter_map(|item| match item {
                UserInput::LocalPath { path, content } => {
                    let path = path.to_string_lossy();
                    let path = codex_utils_string::xml_attribute(&path);
                    Some(
                        approx_token_count(&format!("<local_path_context path=\"{path}\">\n"))
                            .saturating_add(approx_token_count(
                                &codex_utils_string::xml_text(content).to_string(),
                            ))
                            .saturating_add(approx_token_count("\n</local_path_context>")),
                    )
                }
                _ => None,
            })
            .collect();
        let path_count = path_demands.len();
        let mut remaining_path_tokens = LOCAL_PATH_CONTEXT_TOKEN_BUDGET;
        // Preserve the existing marginal-overage behavior for one selected path;
        // multiple paths must not multiply that allowance.
        let retry_margin = if path_count == 1 {
            LOCAL_PATH_CONTEXT_RETRY_AVOIDANCE_TOKEN_MARGIN
        } else {
            0
        };
        let omit_path_group = path_count
            > LOCAL_PATH_CONTEXT_TOKEN_BUDGET
                / approx_token_count(LOCAL_PATH_CONTEXT_METADATA_OMISSION);
        let mut path_group_omission_emitted = false;
        // Allocate before rendering so presentation order loses no evidence.
        let mut demand_order: Vec<usize> = (0..path_count).collect();
        demand_order.sort_unstable_by_key(|&index| path_demands[index]);
        let mut path_budgets = vec![0; path_count];
        for (position, &index) in demand_order.iter().enumerate() {
            let share = remaining_path_tokens / (path_count - position);
            if path_demands[index] > share {
                for &remaining_index in &demand_order[position..] {
                    path_budgets[remaining_index] = share;
                }
                break;
            }
            path_budgets[index] = path_demands[index];
            remaining_path_tokens -= path_demands[index];
        }
        let mut path_budgets = path_budgets.into_iter();
        Self::Message {
            role: "user".to_string(),
            content: items
                .into_iter()
                .flat_map(|c| match c {
                    UserInput::Text { text, .. } => vec![ContentItem::InputText { text }],
                    UserInput::Image {
                        image_url, detail, ..
                    } => {
                        image_index += 1;
                        let detail = detail.unwrap_or(DEFAULT_IMAGE_DETAIL);
                        vec![ContentItem::InputImage {
                            image_url,
                            detail: Some(detail),
                        }]
                    }
                    UserInput::LocalImage { path, detail, .. } => {
                        image_index += 1;
                        let detail = detail.unwrap_or(DEFAULT_IMAGE_DETAIL);
                        local_image(&path, image_index, detail)
                    }
                    UserInput::LocalPath { path, content } => {
                        if omit_path_group {
                            if path_group_omission_emitted {
                                return Vec::new();
                            }
                            path_group_omission_emitted = true;
                            return vec![ContentItem::InputText {
                                text: LOCAL_PATH_CONTEXT_METADATA_OMISSION.to_string(),
                            }];
                        }
                        let text = render_local_path_context(
                            &path,
                            &content,
                            path_budgets.next().unwrap_or_default(),
                            retry_margin,
                        );
                        vec![ContentItem::InputText { text }]
                    }
                    UserInput::Skill { .. } | UserInput::Mention { .. } => Vec::new(), // Tool bodies are injected later in core
                })
                .collect::<Vec<ContentItem>>(),
            phase: None,
        }
    }
}
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SearchToolCallParams {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limit: Option<usize>,
}

impl<'de> Deserialize<'de> for SearchToolCallParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Params {
            query: String,
            #[serde(default)]
            limit: Option<usize>,
        }
        let params = Params::deserialize(deserializer).map_err(|error| {
            serde::de::Error::custom(format!(
                "{error}. tool_search accepts only query and limit; to restrict a provider, put source:<canonical namespace> in query."
            ))
        })?;
        Ok(Self {
            query: params.query,
            limit: params.limit,
        })
    }
}

/// If the `name` of a `ResponseItem::FunctionCall` is `shell_command`, the
/// `arguments` field should deserialize to this struct.
#[derive(Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
pub struct ShellCommandToolCallParams {
    /// Optional model-visible output token budget, capped by policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub max_output_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub program: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub script_body: Option<String>,
    pub workdir: Option<String>,

    /// Whether to run the shell with login shell semantics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<bool>,
    /// This is the maximum time in milliseconds that the command is allowed to run.
    #[serde(alias = "timeout")]
    pub timeout_ms: Option<u64>,
    /// Maximum time a running command may produce no stdout or stderr before it is
    /// treated as stalled. A value of zero disables progress-based stall detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stall_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sandbox_permissions: Option<SandboxPermissions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub prefix_rule: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Execute normally rather than reusing prior immutable evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub force_fresh: Option<bool>,
    /// Required for commands recognized as tests, checks, lints, benchmarks,
    /// or fuzzers so the harness can scope and reuse their proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub validation: Option<crate::validation::ValidationCommandContext>,
}

/// Responses API compatible content items that can be returned by a tool call.
/// This is a subset of ContentItem with the types we support as function call outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FunctionCallOutputContentItem {
    // Do not rename, these are serialized and used directly in the responses API.
    InputText {
        text: String,
    },
    // Do not rename, these are serialized and used directly in the responses API.
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },
    EncryptedContent {
        encrypted_content: String,
    },
}

/// Converts structured function-call output content into plain text for
/// human-readable surfaces.
///
/// This conversion is intentionally lossy:
/// - only `input_text` items are included
/// - image items are ignored
///
/// We use this helper where callers still need a string representation (for
/// example telemetry previews or legacy string-only output paths) while keeping
/// the original multimodal `content_items` as the authoritative payload sent to
/// the model.
pub fn function_call_output_content_items_to_text(
    content_items: &[FunctionCallOutputContentItem],
) -> Option<String> {
    let text_segments = content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
                Some(text.as_str())
            }
            FunctionCallOutputContentItem::InputText { .. }
            | FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect::<Vec<_>>();

    if text_segments.is_empty() {
        None
    } else {
        Some(text_segments.join("\n"))
    }
}

impl From<crate::dynamic_tools::DynamicToolCallOutputContentItem>
    for FunctionCallOutputContentItem
{
    fn from(item: crate::dynamic_tools::DynamicToolCallOutputContentItem) -> Self {
        match item {
            crate::dynamic_tools::DynamicToolCallOutputContentItem::InputText { text } => {
                Self::InputText { text }
            }
            crate::dynamic_tools::DynamicToolCallOutputContentItem::InputImage { image_url } => {
                Self::InputImage {
                    image_url,
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                }
            }
        }
    }
}

/// The payload we send back to OpenAI when reporting a tool call result.
///
/// `body` serializes directly as the wire value for `function_call_output.output`.
/// `success` remains internal metadata for downstream handling.
#[derive(Debug, Default, Clone, PartialEq, JsonSchema, TS)]
pub struct FunctionCallOutputPayload {
    pub body: FunctionCallOutputBody,
    pub success: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(untagged)]
pub enum FunctionCallOutputBody {
    Text(String),
    ContentItems(Vec<FunctionCallOutputContentItem>),
}

impl FunctionCallOutputBody {
    /// Best-effort conversion of a function-call output body to plain text for
    /// human-readable surfaces.
    ///
    /// This conversion is intentionally lossy when the body contains content
    /// items: image entries are dropped and text entries are joined with
    /// newlines.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Self::Text(content) => Some(content.clone()),
            Self::ContentItems(items) => function_call_output_content_items_to_text(items),
        }
    }
}

impl Default for FunctionCallOutputBody {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl FunctionCallOutputPayload {
    pub fn from_text(content: String) -> Self {
        Self {
            body: FunctionCallOutputBody::Text(content),
            success: None,
        }
    }

    pub fn from_content_items(content_items: Vec<FunctionCallOutputContentItem>) -> Self {
        Self {
            body: FunctionCallOutputBody::ContentItems(content_items),
            success: None,
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        match &self.body {
            FunctionCallOutputBody::Text(content) => Some(content),
            FunctionCallOutputBody::ContentItems(_) => None,
        }
    }

    pub fn text_content_mut(&mut self) -> Option<&mut String> {
        match &mut self.body {
            FunctionCallOutputBody::Text(content) => Some(content),
            FunctionCallOutputBody::ContentItems(_) => None,
        }
    }

    pub fn content_items(&self) -> Option<&[FunctionCallOutputContentItem]> {
        match &self.body {
            FunctionCallOutputBody::Text(_) => None,
            FunctionCallOutputBody::ContentItems(items) => Some(items),
        }
    }

    pub fn content_items_mut(&mut self) -> Option<&mut Vec<FunctionCallOutputContentItem>> {
        match &mut self.body {
            FunctionCallOutputBody::Text(_) => None,
            FunctionCallOutputBody::ContentItems(items) => Some(items),
        }
    }
}

// `function_call_output.output` is encoded as either:
//   - an array of structured content items
//   - a plain string
impl Serialize for FunctionCallOutputPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.body {
            FunctionCallOutputBody::Text(content) => serializer.serialize_str(content),
            FunctionCallOutputBody::ContentItems(items) => items.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FunctionCallOutputPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let body = FunctionCallOutputBody::deserialize(deserializer)?;
        Ok(FunctionCallOutputPayload {
            body,
            success: None,
        })
    }
}

impl CallToolResult {
    pub fn from_result(result: Result<Self, String>) -> Self {
        match result {
            Ok(result) => result,
            Err(error) => Self::from_error_text(error),
        }
    }

    pub fn from_error_text(text: String) -> Self {
        Self {
            content: vec![serde_json::json!({
                "type": "text",
                "text": text,
            })],
            structured_content: None,
            is_error: Some(true),
            meta: None,
        }
    }

    pub fn success(&self) -> bool {
        self.is_error != Some(true)
    }

    pub fn as_function_call_output_payload(&self) -> FunctionCallOutputPayload {
        mcp_items_to_function_call_output_payload(
            convert_mcp_content_to_items(&self.content).unwrap_or_default(),
            self.structured_content.as_ref(),
            self.success(),
        )
    }

    pub fn into_function_call_output_payload(self) -> FunctionCallOutputPayload {
        let success = self.success();
        let items = self
            .content
            .into_iter()
            .map(|mut content| {
                let parsed = take_mcp_content(&mut content);
                convert_mcp_content_item(parsed, Cow::Owned(content))
            })
            .collect();
        mcp_items_to_function_call_output_payload(
            items,
            self.structured_content.as_ref(),
            success,
        )
    }
}

fn mcp_items_to_function_call_output_payload(
    mut items: Vec<FunctionCallOutputContentItem>,
    structured_content: Option<&serde_json::Value>,
    success: bool,
) -> FunctionCallOutputPayload {
    if let Some(structured) = structured_content.filter(|value| !value.is_null()) {
        // MCP servers commonly include a JSON text mirror for older clients.
        // Only that exact representation is redundant; captions and images are not.
        items.retain(|item| !matches!(item,
            FunctionCallOutputContentItem::InputText { text }
                if serde_json::from_str::<serde_json::Value>(text).ok().as_ref() == Some(structured)
        ));
        items.push(FunctionCallOutputContentItem::InputText {
            text: structured.to_string(),
        });
    }
    if !success {
        items.insert(
            0,
            FunctionCallOutputContentItem::InputText {
                text: "MCP tool reported an error.".to_string(),
            },
        );
    }
    let text_bytes = items
        .iter()
        .try_fold(items.len().saturating_sub(1), |bytes, item| match item {
            FunctionCallOutputContentItem::InputText { text } => {
                Some(bytes.saturating_add(text.len()))
            }
            _ => None,
        });
    let body = if let Some(text_bytes) = text_bytes {
        let mut texts = items.into_iter().filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text),
            _ => None,
        });
        let mut text = texts.next().unwrap_or_default();
        text.reserve(text_bytes.saturating_sub(text.len()));
        for next in texts {
            text.push('\n');
            text.push_str(&next);
        }
        FunctionCallOutputBody::Text(text)
    } else {
        FunctionCallOutputBody::ContentItems(items)
    };

    FunctionCallOutputPayload {
        body,
        success: Some(success),
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum McpContent {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(rename = "_meta", default)]
        meta: Option<serde_json::Value>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: Option<String>,
        #[serde(rename = "_meta", default)]
        meta: Option<serde_json::Value>,
    },
    #[serde(other)]
    Unknown,
}

/// Validate before moving fields so malformed content remains intact for the
/// same public-JSON fallback used by the borrowed conversion.
fn take_mcp_content(content: &mut serde_json::Value) -> Option<McpContent> {
    let object = content.as_object_mut()?;
    let is_text = match object.get("type")?.as_str()? {
        "text" => true,
        "image" => false,
        _ => return Some(McpContent::Unknown),
    };
    if is_text {
        if !object.get("text").is_some_and(serde_json::Value::is_string) {
            return None;
        }
        let text = match object.remove("text")? {
            serde_json::Value::String(text) => text,
            _ => return None,
        };
        return Some(McpContent::Text {
            text,
            meta: object.remove("_meta"),
        });
    }
    // Serde rejects duplicate aliases even when one value is null.
    if !object.get("data").is_some_and(serde_json::Value::is_string)
        || (object.contains_key("mimeType") && object.contains_key("mime_type"))
        || object
            .get("mimeType")
            .or_else(|| object.get("mime_type"))
            .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return None;
    }
    let data = match object.remove("data")? {
        serde_json::Value::String(data) => data,
        _ => return None,
    };
    let mime_type = match object
        .remove("mimeType")
        .or_else(|| object.remove("mime_type"))
    {
        Some(serde_json::Value::String(mime_type)) => Some(mime_type),
        _ => None,
    };
    Some(McpContent::Image {
        data,
        mime_type,
        meta: object.remove("_meta"),
    })
}

fn convert_mcp_content_to_items(
    contents: &[serde_json::Value],
) -> Option<Vec<FunctionCallOutputContentItem>> {
    Some(
        contents
            .iter()
            .map(|content| {
                convert_mcp_content_item(
                    McpContent::deserialize(content).ok(),
                    Cow::Borrowed(content),
                )
            })
            .collect(),
    )
}

fn convert_mcp_content_item(
    parsed: Option<McpContent>,
    mut content: Cow<'_, serde_json::Value>,
) -> FunctionCallOutputContentItem {
    const CODEX_ENCRYPTED_CONTENT_META_KEY: &str = "codex/encryptedContent";
    const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";

    match parsed {
        Some(McpContent::Text { text, meta }) => {
            if meta
                .as_ref()
                .and_then(|meta| meta.get(CODEX_ENCRYPTED_CONTENT_META_KEY))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: text,
                }
            } else {
                FunctionCallOutputContentItem::InputText { text }
            }
        }
        Some(McpContent::Image {
            data,
            mime_type,
            meta,
        }) => {
            let image_url = if data.starts_with("data:") {
                data
            } else {
                let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
                format!("data:{mime_type};base64,{data}")
            };
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: meta
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                    .and_then(|meta| meta.get(CODEX_IMAGE_DETAIL_META_KEY))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|detail| match detail {
                        "auto" => Some(ImageDetail::Auto),
                        "low" => Some(ImageDetail::Low),
                        "high" => Some(ImageDetail::High),
                        "original" => Some(ImageDetail::Original),
                        _ => None,
                    })
                    .or(Some(DEFAULT_IMAGE_DETAIL)),
            }
        }
        Some(McpContent::Unknown) | None => {
            if let Some(object) = content.to_mut().as_object_mut() {
                object.remove("_meta");
            }
            FunctionCallOutputContentItem::InputText {
                text: content.to_string(),
            }
        }
    }
}

// Implement Display so callers can treat the payload like a plain string for logging or
// substring checks. For `ContentItems`, Display emits a JSON representation.

impl std::fmt::Display for FunctionCallOutputPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.body {
            FunctionCallOutputBody::Text(content) => f.write_str(content),
            FunctionCallOutputBody::ContentItems(items) => {
                let content = serde_json::to_string(items).unwrap_or_default();
                f.write_str(content.as_str())
            }
        }
    }
}

// (Moved event mapping logic into codex-core to avoid coupling protocol to UI-facing events.)

