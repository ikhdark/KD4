use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error as _;
use serde::de::value::Error as ValueDeserializerError;
use serde::de::value::StrDeserializer;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use wildmatch::WildMatchPattern;

use super::requirements_exec_policy::RequirementsExecPolicy;
use super::requirements_exec_policy::RequirementsExecPolicyToml;
use crate::Constrained;
use crate::ConstraintError;
use crate::ManagedHooksRequirementsToml;
use crate::mcp_requirements::McpServerRequirement;
use crate::mcp_types::AppToolApproval;
pub use crate::permissions_toml::NetworkDomainPermissionToml;
pub use crate::permissions_toml::NetworkDomainPermissionsToml;
pub use crate::permissions_toml::NetworkUnixSocketPermissionToml;
pub use crate::permissions_toml::NetworkUnixSocketPermissionsToml;
use crate::permissions_toml::PermissionProfileToml;
use crate::types::WindowsSandboxModeToml;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    Unknown,
    MdmManagedPreferences {
        domain: String,
        key: String,
    },
    /// Multiple requirements layers contributed to the final value. Sources are
    /// stored highest-priority first, matching the order surfaced in errors.
    Composite {
        sources: Vec<RequirementSource>,
    },
    /// A backend-delivered enterprise-managed layer. `id` is the stable backend
    /// identifier; `name` is the admin-facing display name.
    EnterpriseManaged {
        id: String,
        name: String,
    },
    SystemRequirementsToml {
        file: AbsolutePathBuf,
    },
    LegacyManagedConfigTomlFromFile {
        file: AbsolutePathBuf,
    },
    LegacyManagedConfigTomlFromMdm,
}

impl RequirementSource {
    pub fn composite(sources: impl IntoIterator<Item = RequirementSource>) -> Self {
        let mut flattened = Vec::new();
        for source in sources {
            source.append_to_composite(&mut flattened);
        }

        match flattened.len() {
            0 => RequirementSource::Unknown,
            1 => flattened.remove(0),
            _ => RequirementSource::Composite { sources: flattened },
        }
    }

    fn append_to_composite(self, flattened: &mut Vec<RequirementSource>) {
        match self {
            RequirementSource::Composite { sources } => {
                for source in sources {
                    source.append_to_composite(flattened);
                }
            }
            source => {
                if !flattened.contains(&source) {
                    flattened.push(source);
                }
            }
        }
    }
}

impl fmt::Display for RequirementSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequirementSource::Unknown => write!(f, "<unspecified>"),
            RequirementSource::MdmManagedPreferences { domain, key } => {
                write!(f, "MDM {domain}:{key}")
            }
            RequirementSource::Composite { sources } => {
                write!(f, "requirements layers: ")?;
                for (index, source) in sources.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{source}")?;
                }
                Ok(())
            }
            RequirementSource::EnterpriseManaged { id, name } => {
                write!(f, "enterprise-managed requirements {name} ({id})")
            }
            RequirementSource::SystemRequirementsToml { file } => {
                write!(f, "{}", file.as_path().display())
            }
            RequirementSource::LegacyManagedConfigTomlFromFile { file } => {
                write!(f, "{}", file.as_path().display())
            }
            RequirementSource::LegacyManagedConfigTomlFromMdm => {
                write!(f, "MDM managed_config.toml (legacy)")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstrainedWithSource<T> {
    pub value: Constrained<T>,
    pub source: Option<RequirementSource>,
}

impl<T> ConstrainedWithSource<T> {
    pub fn new(value: Constrained<T>, source: Option<RequirementSource>) -> Self {
        Self { value, source }
    }
}

impl<T> std::ops::Deref for ConstrainedWithSource<T> {
    type Target = Constrained<T>;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> std::ops::DerefMut for ConstrainedWithSource<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

/// Normalized version of [`ConfigRequirementsToml`] after deserialization and
/// normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigRequirements {
    pub approval_policy: ConstrainedWithSource<AskForApproval>,
    pub permission_profile: ConstrainedWithSource<PermissionProfile>,
    pub windows_sandbox_mode: ConstrainedWithSource<Option<WindowsSandboxModeToml>>,
    pub web_search_mode: ConstrainedWithSource<WebSearchMode>,
    pub allow_managed_hooks_only: Option<Sourced<bool>>,
    pub allow_appshots: Option<Sourced<bool>>,
    pub allow_remote_control: Option<Sourced<bool>>,
    pub computer_use: Option<Sourced<ComputerUseRequirementsToml>>,
    pub feature_requirements: Option<Sourced<FeatureRequirementsToml>>,
    pub managed_hooks: Option<ConstrainedWithSource<ManagedHooksRequirementsToml>>,
    pub mcp_servers: Option<Sourced<BTreeMap<String, McpServerRequirement>>>,
    pub plugins: Option<Sourced<BTreeMap<String, PluginRequirementsToml>>>,
    pub marketplaces: Option<Sourced<MarketplaceRequirementsToml>>,
    pub exec_policy: Option<Sourced<RequirementsExecPolicy>>,
    pub enforce_residency: ConstrainedWithSource<Option<ResidencyRequirement>>,
    /// Managed network constraints derived from requirements.
    pub network: Option<Sourced<NetworkConstraints>>,
    /// Managed filesystem constraints derived from requirements.
    pub filesystem: Option<Sourced<FilesystemConstraints>>,
}

impl Default for ConfigRequirements {
    fn default() -> Self {
        Self {
            approval_policy: ConstrainedWithSource::new(
                Constrained::allow_any_from_default(),
                /*source*/ None,
            ),
            permission_profile: ConstrainedWithSource::new(
                Constrained::allow_any(PermissionProfile::read_only()),
                /*source*/ None,
            ),
            windows_sandbox_mode: ConstrainedWithSource::new(
                Constrained::allow_any(/*initial_value*/ None),
                /*source*/ None,
            ),
            web_search_mode: ConstrainedWithSource::new(
                Constrained::allow_any(WebSearchMode::Cached),
                /*source*/ None,
            ),
            allow_managed_hooks_only: None,
            allow_appshots: None,
            allow_remote_control: None,
            computer_use: None,
            feature_requirements: None,
            managed_hooks: None,
            mcp_servers: None,
            plugins: None,
            marketplaces: None,
            exec_policy: None,
            enforce_residency: ConstrainedWithSource::new(
                Constrained::allow_any(/*initial_value*/ None),
                /*source*/ None,
            ),
            network: None,
            filesystem: None,
        }
    }
}

impl ConfigRequirements {
    pub fn exec_policy_source(&self) -> Option<&RequirementSource> {
        self.exec_policy.as_ref().map(|policy| &policy.source)
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRequirementsToml {
    pub mcp_servers: Option<BTreeMap<String, McpServerRequirement>>,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceRequirementsToml {
    pub restrict_to_allowed_sources: Option<bool>,
    #[serde(default)]
    pub allowed_sources: BTreeMap<String, MarketplaceAllowedSourceToml>,
}

impl MarketplaceRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.restrict_to_allowed_sources.is_none() && self.allowed_sources.is_empty()
    }
}

/// Raw marketplace source rule whose active fields are interpreted after
/// requirements composition.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceAllowedSourceToml {
    pub source: Option<MarketplaceAllowedSourceKind>,
    pub url: Option<String>,
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    pub host_pattern: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketplaceAllowedSourceKind {
    Git,
    HostPattern,
    Local,
}

impl PluginRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.as_ref().is_none_or(BTreeMap::is_empty)
    }
}

#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkRequirementsToml {
    pub enabled: Option<bool>,
    pub http_port: Option<u16>,
    pub socks_port: Option<u16>,
    pub allow_upstream_proxy: Option<bool>,
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    pub domains: Option<NetworkDomainPermissionsToml>,
    /// When true, only managed `allowed_domains` are respected while managed
    /// network enforcement is active. User allowlist entries are ignored.
    pub managed_allowed_domains_only: Option<bool>,
    pub unix_sockets: Option<NetworkUnixSocketPermissionsToml>,
    pub allow_local_binding: Option<bool>,
}

#[derive(Deserialize)]
struct RawNetworkRequirementsToml {
    enabled: Option<bool>,
    http_port: Option<u16>,
    socks_port: Option<u16>,
    allow_upstream_proxy: Option<bool>,
    dangerously_allow_non_loopback_proxy: Option<bool>,
    dangerously_allow_all_unix_sockets: Option<bool>,
    domains: Option<NetworkDomainPermissionsToml>,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    /// When true, only managed `allowed_domains` are respected while managed
    /// network enforcement is active. User allowlist entries are ignored.
    managed_allowed_domains_only: Option<bool>,
    #[serde(default)]
    denied_domains: Option<Vec<String>>,
    unix_sockets: Option<NetworkUnixSocketPermissionsToml>,
    #[serde(default)]
    allow_unix_sockets: Option<Vec<String>>,
    allow_local_binding: Option<bool>,
}

impl<'de> Deserialize<'de> for NetworkRequirementsToml {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawNetworkRequirementsToml::deserialize(deserializer)?;
        let RawNetworkRequirementsToml {
            enabled,
            http_port,
            socks_port,
            allow_upstream_proxy,
            dangerously_allow_non_loopback_proxy,
            dangerously_allow_all_unix_sockets,
            domains,
            allowed_domains,
            managed_allowed_domains_only,
            denied_domains,
            unix_sockets,
            allow_unix_sockets,
            allow_local_binding,
        } = raw;

        if domains.is_some() && (allowed_domains.is_some() || denied_domains.is_some()) {
            return Err(D::Error::custom(
                "`experimental_network.domains` cannot be combined with legacy `allowed_domains` or `denied_domains`",
            ));
        }

        if unix_sockets.is_some() && allow_unix_sockets.is_some() {
            return Err(D::Error::custom(
                "`experimental_network.unix_sockets` cannot be combined with legacy `allow_unix_sockets`",
            ));
        }

        Ok(Self {
            enabled,
            http_port,
            socks_port,
            allow_upstream_proxy,
            dangerously_allow_non_loopback_proxy,
            dangerously_allow_all_unix_sockets,
            domains: domains
                .or_else(|| legacy_domain_permissions_from_lists(allowed_domains, denied_domains)),
            managed_allowed_domains_only,
            unix_sockets: unix_sockets
                .or_else(|| legacy_unix_socket_permissions_from_list(allow_unix_sockets)),
            allow_local_binding,
        })
    }
}

/// Legacy list normalization is intentionally lossy: explicit empty legacy
/// lists are treated as unset when converted to the canonical network
/// permission shape.
fn legacy_domain_permissions_from_lists(
    allowed_domains: Option<Vec<String>>,
    denied_domains: Option<Vec<String>>,
) -> Option<NetworkDomainPermissionsToml> {
    let mut entries = BTreeMap::new();

    for pattern in allowed_domains.unwrap_or_default() {
        entries.insert(pattern, NetworkDomainPermissionToml::Allow);
    }

    for pattern in denied_domains.unwrap_or_default() {
        entries.insert(pattern, NetworkDomainPermissionToml::Deny);
    }

    (!entries.is_empty()).then_some(NetworkDomainPermissionsToml { entries })
}

fn legacy_unix_socket_permissions_from_list(
    allow_unix_sockets: Option<Vec<String>>,
) -> Option<NetworkUnixSocketPermissionsToml> {
    let entries = allow_unix_sockets
        .unwrap_or_default()
        .into_iter()
        .map(|path| (path, NetworkUnixSocketPermissionToml::Allow))
        .collect::<BTreeMap<_, _>>();

    (!entries.is_empty()).then_some(NetworkUnixSocketPermissionsToml { entries })
}

/// Normalized network constraints derived from requirements TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NetworkConstraints {
    pub enabled: Option<bool>,
    pub http_port: Option<u16>,
    pub socks_port: Option<u16>,
    pub allow_upstream_proxy: Option<bool>,
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    pub domains: Option<NetworkDomainPermissionsToml>,
    /// When true, only managed `allowed_domains` are respected while managed
    /// network enforcement is active. User allowlist entries are ignored.
    pub managed_allowed_domains_only: Option<bool>,
    pub unix_sockets: Option<NetworkUnixSocketPermissionsToml>,
    pub allow_local_binding: Option<bool>,
}

impl<'de> Deserialize<'de> for NetworkConstraints {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let requirements = NetworkRequirementsToml::deserialize(deserializer)?;
        Ok(requirements.into())
    }
}

impl From<NetworkRequirementsToml> for NetworkConstraints {
    fn from(value: NetworkRequirementsToml) -> Self {
        let NetworkRequirementsToml {
            enabled,
            http_port,
            socks_port,
            allow_upstream_proxy,
            dangerously_allow_non_loopback_proxy,
            dangerously_allow_all_unix_sockets,
            domains,
            managed_allowed_domains_only,
            unix_sockets,
            allow_local_binding,
        } = value;
        Self {
            enabled,
            http_port,
            socks_port,
            allow_upstream_proxy,
            dangerously_allow_non_loopback_proxy,
            dangerously_allow_all_unix_sockets,
            domains,
            managed_allowed_domains_only,
            unix_sockets,
            allow_local_binding,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilesystemRequirementsToml {
    pub deny_read: Option<Vec<FilesystemDenyReadPattern>>,
}

#[derive(Deserialize)]
struct RawFilesystemRequirementsToml {
    deny_read: Option<Vec<FilesystemDenyReadPattern>>,
    description: Option<serde::de::IgnoredAny>,
    extends: Option<serde::de::IgnoredAny>,
    workspace_roots: Option<serde::de::IgnoredAny>,
    filesystem: Option<serde::de::IgnoredAny>,
    network: Option<serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for FilesystemRequirementsToml {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawFilesystemRequirementsToml::deserialize(deserializer)?;
        let RawFilesystemRequirementsToml {
            deny_read,
            description,
            extends,
            workspace_roots,
            filesystem,
            network,
        } = raw;

        if description.is_some()
            || extends.is_some()
            || workspace_roots.is_some()
            || filesystem.is_some()
            || network.is_some()
        {
            return Err(D::Error::custom(
                "`permissions.filesystem` is reserved for requirements-level filesystem constraints and cannot define a profile",
            ));
        }

        Ok(Self { deny_read })
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionsRequirementsToml {
    pub filesystem: Option<FilesystemRequirementsToml>,
    // For legacy reasons, `filesystem` stays reserved for requirements-level
    // filesystem constraints and cannot name a profile.
    #[serde(default, flatten)]
    pub profiles: BTreeMap<String, PermissionProfileToml>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemConstraints {
    pub deny_read: Vec<FilesystemDenyReadPattern>,
}

impl From<PermissionsRequirementsToml> for FilesystemConstraints {
    fn from(value: PermissionsRequirementsToml) -> Self {
        let deny_read = value
            .filesystem
            .and_then(|filesystem| filesystem.deny_read)
            .unwrap_or_default();
        Self { deny_read }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FilesystemDenyReadPattern(String);

impl FilesystemDenyReadPattern {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn contains_glob(&self) -> bool {
        first_glob_metacharacter(&self.0).is_some()
    }

    pub fn from_input(input: &str) -> Result<Self, String> {
        if first_glob_metacharacter(input).is_none() {
            let path = deserialize_absolute_path(input)?;
            return Ok(Self(path.to_string_lossy().into_owned()));
        }

        let (directory_prefix, suffix) = split_glob_pattern(input);
        let normalized_prefix = if directory_prefix.is_empty() {
            deserialize_absolute_path(".")?
        } else {
            deserialize_absolute_path(directory_prefix)?
        };
        let normalized_prefix = normalized_prefix.to_string_lossy();
        let normalized = if suffix.is_empty() {
            normalized_prefix.into_owned()
        } else if normalized_prefix == "/" {
            format!("/{suffix}")
        } else {
            format!("{normalized_prefix}/{suffix}")
        };
        Ok(Self(normalized))
    }
}

impl From<AbsolutePathBuf> for FilesystemDenyReadPattern {
    fn from(value: AbsolutePathBuf) -> Self {
        Self(value.to_string_lossy().into_owned())
    }
}

impl<'de> Deserialize<'de> for FilesystemDenyReadPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input = String::deserialize(deserializer)?;
        Self::from_input(&input).map_err(D::Error::custom)
    }
}

fn deserialize_absolute_path(input: &str) -> Result<AbsolutePathBuf, String> {
    AbsolutePathBuf::deserialize(StrDeserializer::<ValueDeserializerError>::new(input))
        .map_err(|err| err.to_string())
}

fn split_glob_pattern(input: &str) -> (&str, &str) {
    let Some(first_glob) = first_glob_metacharacter(input) else {
        return ("", input);
    };
    let separator_index = input[..first_glob]
        .char_indices()
        .rev()
        .find(|(_, ch)| is_path_separator(*ch))
        .map(|(index, _)| index);

    match separator_index {
        Some(0) => ("/", &input[1..]),
        Some(index)
            if (index == 2 || (index == 6 && input.starts_with(r"\\?\")))
                && input.as_bytes().get(index - 1) == Some(&b':')
                && input.as_bytes().get(2).is_some() =>
        {
            (&input[..=index], &input[index + 1..])
        }
        Some(index) => (&input[..index], &input[index + 1..]),
        None => ("", input),
    }
}

fn first_glob_metacharacter(input: &str) -> Option<usize> {
    // The question mark in a Windows verbatim namespace is not a wildcard.
    let namespace_len = if input.starts_with(r"\\?\") { 4 } else { 0 };
    input[namespace_len..]
        .find(is_glob_metacharacter)
        .map(|index| namespace_len + index)
}

fn is_path_separator(ch: char) -> bool {
    ch == '/' || ch == '\\'
}

fn is_glob_metacharacter(ch: char) -> bool {
    matches!(ch, '*' | '?' | '[')
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchModeRequirement {
    Disabled,
    Cached,
    Indexed,
    Live,
}

impl From<WebSearchMode> for WebSearchModeRequirement {
    fn from(mode: WebSearchMode) -> Self {
        match mode {
            WebSearchMode::Disabled => WebSearchModeRequirement::Disabled,
            WebSearchMode::Cached => WebSearchModeRequirement::Cached,
            WebSearchMode::Indexed => WebSearchModeRequirement::Indexed,
            WebSearchMode::Live => WebSearchModeRequirement::Live,
        }
    }
}

impl From<WebSearchModeRequirement> for WebSearchMode {
    fn from(mode: WebSearchModeRequirement) -> Self {
        match mode {
            WebSearchModeRequirement::Disabled => WebSearchMode::Disabled,
            WebSearchModeRequirement::Cached => WebSearchMode::Cached,
            WebSearchModeRequirement::Indexed => WebSearchMode::Indexed,
            WebSearchModeRequirement::Live => WebSearchMode::Live,
        }
    }
}

impl fmt::Display for WebSearchModeRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebSearchModeRequirement::Disabled => write!(f, "disabled"),
            WebSearchModeRequirement::Cached => write!(f, "cached"),
            WebSearchModeRequirement::Indexed => write!(f, "indexed"),
            WebSearchModeRequirement::Live => write!(f, "live"),
        }
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ComputerUseRequirementsToml {
    pub allow_locked_computer_use: Option<bool>,
}

impl ComputerUseRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.allow_locked_computer_use.is_none()
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsRequirementsToml {
    pub allowed_sandbox_implementations: Option<Vec<WindowsSandboxModeToml>>,
}

impl WindowsRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.allowed_sandbox_implementations.is_none()
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureRequirementsToml {
    #[serde(flatten)]
    pub entries: BTreeMap<String, bool>,
}

impl FeatureRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AppToolRequirementToml {
    pub approval_mode: Option<AppToolApproval>,
}

impl AppToolRequirementToml {
    pub fn is_empty(&self) -> bool {
        self.approval_mode.is_none()
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AppToolsRequirementsToml {
    #[serde(default, flatten)]
    pub tools: BTreeMap<String, AppToolRequirementToml>,
}

impl AppToolsRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.tools.values().all(AppToolRequirementToml::is_empty)
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AppRequirementToml {
    pub enabled: Option<bool>,
    pub tools: Option<AppToolsRequirementsToml>,
}

impl AppRequirementToml {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self
                .tools
                .as_ref()
                .is_none_or(AppToolsRequirementsToml::is_empty)
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AppsRequirementsToml {
    #[serde(default, flatten)]
    pub apps: BTreeMap<String, AppRequirementToml>,
}

impl AppsRequirementsToml {
    pub fn is_empty(&self) -> bool {
        self.apps.values().all(AppRequirementToml::is_empty)
    }
}

/// Merge app requirements from a lower-precedence source into an existing higher-precedence set.
/// This lets managed sources (for example Cloud/MDM) enforce setting disablement across layers,
/// while exact tool approval settings keep the higher-precedence value when present.
pub(crate) fn merge_app_requirements_descending(
    base: &mut AppsRequirementsToml,
    incoming: AppsRequirementsToml,
) {
    for (app_id, incoming_requirement) in incoming.apps {
        let base_requirement = base.apps.entry(app_id).or_default();
        let higher_precedence = base_requirement.enabled;
        let lower_precedence = incoming_requirement.enabled;
        base_requirement.enabled =
            if higher_precedence == Some(false) || lower_precedence == Some(false) {
                Some(false)
            } else {
                higher_precedence.or(lower_precedence)
            };

        let Some(incoming_tools) = incoming_requirement.tools else {
            continue;
        };
        let base_tools = base_requirement.tools.get_or_insert_with(Default::default);
        for (tool_name, incoming_tool) in incoming_tools.tools {
            let base_tool = base_tools.tools.entry(tool_name).or_default();
            if base_tool.approval_mode.is_none() {
                base_tool.approval_mode = incoming_tool.approval_mode;
            }
        }
    }
}

/// Base config deserialized from system `requirements.toml` or MDM.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ConfigRequirementsToml {
    pub allowed_approval_policies: Option<Vec<AskForApproval>>,
    pub allowed_sandbox_modes: Option<Vec<SandboxModeRequirement>>,
    pub allowed_permission_profiles: Option<BTreeMap<String, bool>>,
    pub default_permissions: Option<String>,
    pub remote_sandbox_config: Option<Vec<RemoteSandboxConfigToml>>,
    pub allowed_web_search_modes: Option<Vec<WebSearchModeRequirement>>,
    pub allow_managed_hooks_only: Option<bool>,
    pub allow_appshots: Option<bool>,
    pub allow_remote_control: Option<bool>,
    pub computer_use: Option<ComputerUseRequirementsToml>,
    pub windows: Option<WindowsRequirementsToml>,
    #[serde(rename = "features", alias = "feature_requirements")]
    pub feature_requirements: Option<FeatureRequirementsToml>,
    pub hooks: Option<ManagedHooksRequirementsToml>,
    pub mcp_servers: Option<BTreeMap<String, McpServerRequirement>>,
    pub plugins: Option<BTreeMap<String, PluginRequirementsToml>>,
    pub marketplaces: Option<MarketplaceRequirementsToml>,
    pub apps: Option<AppsRequirementsToml>,
    pub rules: Option<RequirementsExecPolicyToml>,
    pub enforce_residency: Option<ResidencyRequirement>,
    #[serde(rename = "experimental_network")]
    pub network: Option<NetworkRequirementsToml>,
    pub permissions: Option<PermissionsRequirementsToml>,
    pub models: Option<ModelsRequirementsToml>,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsRequirementsToml {
    pub new_thread: Option<NewThreadModelDefaultsToml>,
}

impl ModelsRequirementsToml {
    fn is_empty(&self) -> bool {
        self.new_thread
            .as_ref()
            .is_none_or(NewThreadModelDefaultsToml::is_empty)
    }
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct NewThreadModelDefaultsToml {
    pub model: Option<String>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<String>,
}

impl NewThreadModelDefaultsToml {
    fn is_empty(&self) -> bool {
        self.model.is_none() && self.model_reasoning_effort.is_none() && self.service_tier.is_none()
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct RemoteSandboxConfigToml {
    pub hostname_patterns: Vec<String>,
    pub allowed_sandbox_modes: Vec<SandboxModeRequirement>,
}

/// Value paired with the requirement source it came from, for better error
/// messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: RequirementSource,
}

impl<T> Sourced<T> {
    pub fn new(value: T, source: RequirementSource) -> Self {
        Self { value, source }
    }
}

impl<T> std::ops::Deref for Sourced<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigRequirementsWithSources {
    pub allowed_approval_policies: Option<Sourced<Vec<AskForApproval>>>,
    pub allowed_sandbox_modes: Option<Sourced<Vec<SandboxModeRequirement>>>,
    pub allowed_permission_profiles: Option<Sourced<BTreeMap<String, bool>>>,
    pub default_permissions: Option<Sourced<String>>,
    pub allowed_web_search_modes: Option<Sourced<Vec<WebSearchModeRequirement>>>,
    pub allow_managed_hooks_only: Option<Sourced<bool>>,
    pub allow_appshots: Option<Sourced<bool>>,
    pub allow_remote_control: Option<Sourced<bool>>,
    pub computer_use: Option<Sourced<ComputerUseRequirementsToml>>,
    pub windows: Option<Sourced<WindowsRequirementsToml>>,
    pub feature_requirements: Option<Sourced<FeatureRequirementsToml>>,
    pub hooks: Option<Sourced<ManagedHooksRequirementsToml>>,
    pub mcp_servers: Option<Sourced<BTreeMap<String, McpServerRequirement>>>,
    pub plugins: Option<Sourced<BTreeMap<String, PluginRequirementsToml>>>,
    pub marketplaces: Option<Sourced<MarketplaceRequirementsToml>>,
    pub apps: Option<Sourced<AppsRequirementsToml>>,
    pub rules: Option<Sourced<RequirementsExecPolicyToml>>,
    pub enforce_residency: Option<Sourced<ResidencyRequirement>>,
    pub network: Option<Sourced<NetworkRequirementsToml>>,
    pub permissions: Option<Sourced<PermissionsRequirementsToml>>,
    pub models: Option<Sourced<ModelsRequirementsToml>>,
}

impl ConfigRequirementsWithSources {
    /// Whether removing source annotations would yield empty requirements.
    pub fn is_empty(&self) -> bool {
        self.allowed_approval_policies.is_none()
            && self.allowed_sandbox_modes.is_none()
            && self.allowed_permission_profiles.is_none()
            && self.default_permissions.is_none()
            && self.allowed_web_search_modes.is_none()
            && self.allow_managed_hooks_only.is_none()
            && self.allow_appshots.is_none()
            && self.allow_remote_control.is_none()
            && self
                .computer_use
                .as_deref()
                .is_none_or(ComputerUseRequirementsToml::is_empty)
            && self
                .windows
                .as_deref()
                .is_none_or(WindowsRequirementsToml::is_empty)
            && self
                .feature_requirements
                .as_deref()
                .is_none_or(FeatureRequirementsToml::is_empty)
            && self
                .hooks
                .as_deref()
                .is_none_or(ManagedHooksRequirementsToml::is_empty)
            && self.mcp_servers.is_none()
            && self
                .plugins
                .as_deref()
                .is_none_or(|plugins| plugins.values().all(PluginRequirementsToml::is_empty))
            && self
                .marketplaces
                .as_deref()
                .is_none_or(MarketplaceRequirementsToml::is_empty)
            && self
                .apps
                .as_deref()
                .is_none_or(AppsRequirementsToml::is_empty)
            && self.rules.is_none()
            && self.enforce_residency.is_none()
            && self.network.is_none()
            && self.permissions.is_none()
            && self
                .models
                .as_deref()
                .is_none_or(ModelsRequirementsToml::is_empty)
    }

    pub fn merge_unset_fields(&mut self, source: RequirementSource, other: ConfigRequirementsToml) {
        // For every field in `other` that is `Some`, if the corresponding field
        // in `self` is `None`, copy the value from `other` into `self`.
        macro_rules! fill_missing_take {
            ($base:expr, $other:expr, $source:expr, { $($field:ident),+ $(,)? }) => {
                $(
                    if $base.$field.is_none()
                        && let Some(value) = $other.$field.take()
                    {
                        $base.$field = Some(Sourced::new(value, $source.clone()));
                    }
                )+
            };
        }

        // Destructure without `..` so adding fields to `ConfigRequirementsToml`
        // forces this merge logic to be updated.
        let ConfigRequirementsToml {
            allowed_approval_policies: _,
            allowed_sandbox_modes: _,
            allowed_permission_profiles: _,
            default_permissions: _,
            remote_sandbox_config: _,
            allowed_web_search_modes: _,
            allow_managed_hooks_only: _,
            allow_appshots: _,
            allow_remote_control: _,
            computer_use: _,
            windows: _,
            feature_requirements: _,
            hooks: _,
            mcp_servers: _,
            plugins: _,
            marketplaces: _,
            apps: _,
            rules: _,
            enforce_residency: _,
            network: _,
            permissions: _,
            models: _,
        } = &other;

        let mut other = other;
        fill_missing_take!(
            self,
            other,
            source,
            {
                allowed_approval_policies,
                allowed_sandbox_modes,
                allowed_permission_profiles,
                default_permissions,
                allowed_web_search_modes,
                allow_managed_hooks_only,
                allow_appshots,
                allow_remote_control,
                computer_use,
                windows,
                feature_requirements,
                hooks,
                mcp_servers,
                plugins,
                marketplaces,
                rules,
                enforce_residency,
                network,
                permissions,
                models,
            }
        );

        if let Some(incoming_apps) = other.apps.take() {
            if let Some(existing_apps) = self.apps.as_mut() {
                merge_app_requirements_descending(&mut existing_apps.value, incoming_apps);
            } else {
                self.apps = Some(Sourced::new(incoming_apps, source));
            }
        }
    }

    pub fn into_toml(self) -> ConfigRequirementsToml {
        let ConfigRequirementsWithSources {
            allowed_approval_policies,
            allowed_sandbox_modes,
            allowed_permission_profiles,
            default_permissions,
            allowed_web_search_modes,
            allow_managed_hooks_only,
            allow_appshots,
            allow_remote_control,
            computer_use,
            windows,
            feature_requirements,
            hooks,
            mcp_servers,
            plugins,
            marketplaces,
            apps,
            rules,
            enforce_residency,
            network,
            permissions,
            models,
        } = self;
        ConfigRequirementsToml {
            allowed_approval_policies: allowed_approval_policies.map(|sourced| sourced.value),
            allowed_sandbox_modes: allowed_sandbox_modes.map(|sourced| sourced.value),
            allowed_permission_profiles: allowed_permission_profiles.map(|sourced| sourced.value),
            default_permissions: default_permissions.map(|sourced| sourced.value),
            remote_sandbox_config: None,
            allowed_web_search_modes: allowed_web_search_modes.map(|sourced| sourced.value),
            allow_managed_hooks_only: allow_managed_hooks_only.map(|sourced| sourced.value),
            allow_appshots: allow_appshots.map(|sourced| sourced.value),
            allow_remote_control: allow_remote_control.map(|sourced| sourced.value),
            computer_use: computer_use.map(|sourced| sourced.value),
            windows: windows.map(|sourced| sourced.value),
            feature_requirements: feature_requirements.map(|sourced| sourced.value),
            hooks: hooks.map(|sourced| sourced.value),
            mcp_servers: mcp_servers.map(|sourced| sourced.value),
            plugins: plugins.map(|sourced| sourced.value),
            marketplaces: marketplaces.map(|sourced| sourced.value),
            apps: apps.map(|sourced| sourced.value),
            rules: rules.map(|sourced| sourced.value),
            enforce_residency: enforce_residency.map(|sourced| sourced.value),
            network: network.map(|sourced| sourced.value),
            permissions: permissions.map(|sourced| sourced.value),
            models: models.map(|sourced| sourced.value),
        }
    }
}

fn normalize_hostname(hostname: &str) -> Option<String> {
    let hostname = hostname.trim().trim_end_matches('.');
    (!hostname.is_empty()).then(|| hostname.to_ascii_lowercase())
}

fn hostname_matches_any_pattern(hostname: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        normalize_hostname(pattern)
            .map(|pattern| WildMatchPattern::<'*', '?'>::new_case_insensitive(&pattern))
            .is_some_and(|pattern| pattern.matches(hostname))
    })
}

/// Currently, `external-sandbox` is not supported in config.toml, but it is
/// supported through programmatic use.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum SandboxModeRequirement {
    #[serde(rename = "read-only")]
    ReadOnly,

    #[serde(rename = "workspace-write")]
    WorkspaceWrite,

    #[serde(rename = "danger-full-access")]
    DangerFullAccess,

    #[serde(rename = "external-sandbox")]
    ExternalSandbox,
}

impl From<SandboxMode> for SandboxModeRequirement {
    fn from(mode: SandboxMode) -> Self {
        match mode {
            SandboxMode::ReadOnly => SandboxModeRequirement::ReadOnly,
            SandboxMode::WorkspaceWrite => SandboxModeRequirement::WorkspaceWrite,
            SandboxMode::DangerFullAccess => SandboxModeRequirement::DangerFullAccess,
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResidencyRequirement {
    Us,
}

impl ConfigRequirementsToml {
    pub fn apply_remote_sandbox_config(&mut self, hostname: Option<&str>) {
        let Some(remote_sandbox_config) = self.remote_sandbox_config.as_ref() else {
            return;
        };
        let Some(hostname) = hostname.and_then(normalize_hostname) else {
            return;
        };
        let Some(matched_config) = remote_sandbox_config
            .iter()
            .find(|config| hostname_matches_any_pattern(&hostname, &config.hostname_patterns))
        else {
            return;
        };
        self.allowed_sandbox_modes = Some(matched_config.allowed_sandbox_modes.clone());
    }

    pub fn is_empty(&self) -> bool {
        self.allowed_approval_policies.is_none()
            && self.allowed_sandbox_modes.is_none()
            && self.allowed_permission_profiles.is_none()
            && self.default_permissions.is_none()
            && self.remote_sandbox_config.is_none()
            && self.allowed_web_search_modes.is_none()
            && self.allow_managed_hooks_only.is_none()
            && self.allow_appshots.is_none()
            && self.allow_remote_control.is_none()
            && self
                .computer_use
                .as_ref()
                .is_none_or(ComputerUseRequirementsToml::is_empty)
            && self
                .windows
                .as_ref()
                .is_none_or(WindowsRequirementsToml::is_empty)
            && self
                .feature_requirements
                .as_ref()
                .is_none_or(FeatureRequirementsToml::is_empty)
            && self
                .hooks
                .as_ref()
                .is_none_or(ManagedHooksRequirementsToml::is_empty)
            && self.mcp_servers.is_none()
            && self
                .plugins
                .as_ref()
                .is_none_or(|plugins| plugins.values().all(PluginRequirementsToml::is_empty))
            && self
                .marketplaces
                .as_ref()
                .is_none_or(MarketplaceRequirementsToml::is_empty)
            && self
                .apps
                .as_ref()
                .is_none_or(AppsRequirementsToml::is_empty)
            && self.rules.is_none()
            && self.enforce_residency.is_none()
            && self.network.is_none()
            && self.permissions.is_none()
            && self
                .models
                .as_ref()
                .is_none_or(ModelsRequirementsToml::is_empty)
    }
}

fn validate_mcp_server_requirements(
    requirements: &BTreeMap<String, McpServerRequirement>,
    source: &RequirementSource,
    plugin_name: Option<&str>,
) -> Result<(), ConstraintError> {
    for (server_name, requirement) in requirements {
        requirement
            .validate()
            .map_err(|reason| ConstraintError::McpServerRequirementParse {
                server_name: plugin_name
                    .map(|plugin_name| format!("{plugin_name}/{server_name}"))
                    .unwrap_or_else(|| server_name.clone()),
                requirement_source: source.clone(),
                reason,
            })?;
    }
    Ok(())
}

impl TryFrom<ConfigRequirementsWithSources> for ConfigRequirements {
    type Error = ConstraintError;

    fn try_from(toml: ConfigRequirementsWithSources) -> Result<Self, Self::Error> {
        // Profile catalog selection remains on ConfigRequirementsToml for
        // config loading and requirements API projection. Managed new-thread
        // defaults also remain there because they are initialization values,
        // not runtime constraints.
        let ConfigRequirementsWithSources {
            allowed_approval_policies,
            allowed_sandbox_modes,
            allowed_permission_profiles: _,
            default_permissions: _,
            allowed_web_search_modes,
            allow_managed_hooks_only,
            allow_appshots,
            allow_remote_control,
            computer_use,
            windows,
            feature_requirements,
            hooks,
            mcp_servers,
            plugins,
            marketplaces,
            apps: _apps,
            rules,
            enforce_residency,
            network,
            permissions,
            models: _,
        } = toml;

        if let Some(requirements) = &mcp_servers {
            validate_mcp_server_requirements(
                &requirements.value,
                &requirements.source,
                /*plugin_name*/ None,
            )?;
        }
        if let Some(plugin_requirements) = &plugins {
            for (plugin_name, plugin) in &plugin_requirements.value {
                if let Some(requirements) = &plugin.mcp_servers {
                    validate_mcp_server_requirements(
                        requirements,
                        &plugin_requirements.source,
                        Some(plugin_name),
                    )?;
                }
            }
        }

        let approval_policy = match allowed_approval_policies {
            Some(Sourced {
                value: policies,
                source: requirement_source,
            }) => {
                let Some(initial_value) = policies.first().copied() else {
                    return Err(ConstraintError::empty_field("allowed_approval_policies"));
                };

                let requirement_source_for_error = requirement_source.clone();
                let constrained = Constrained::new(initial_value, move |candidate| {
                    if policies.contains(candidate) {
                        Ok(())
                    } else {
                        Err(ConstraintError::InvalidValue {
                            field_name: "approval_policy",
                            candidate: format!("{candidate:?}"),
                            allowed: format!("{policies:?}"),
                            requirement_source: requirement_source_for_error.clone(),
                        })
                    }
                })?;
                ConstrainedWithSource::new(constrained, Some(requirement_source))
            }
            None => ConstrainedWithSource::new(
                Constrained::allow_any_from_default(),
                /*source*/ None,
            ),
        };

        let default_permission_profile = PermissionProfile::read_only();
        let permission_profile = match allowed_sandbox_modes {
            Some(Sourced {
                value: modes,
                source: requirement_source,
            }) => {
                if !modes.contains(&SandboxModeRequirement::ReadOnly) {
                    return Err(ConstraintError::InvalidValue {
                        field_name: "allowed_sandbox_modes",
                        candidate: format!("{modes:?}"),
                        allowed: "must include 'read-only' to allow any PermissionProfile"
                            .to_string(),
                        requirement_source,
                    });
                };

                let requirement_source_for_error = requirement_source.clone();
                let constrained = Constrained::new(default_permission_profile, move |candidate| {
                    let mode = sandbox_mode_requirement_for_permission_profile(candidate);
                    if modes.contains(&mode) {
                        Ok(())
                    } else {
                        Err(ConstraintError::InvalidValue {
                            field_name: "sandbox_mode",
                            candidate: format!("{mode:?}"),
                            allowed: format!("{modes:?}"),
                            requirement_source: requirement_source_for_error.clone(),
                        })
                    }
                })?;
                ConstrainedWithSource::new(constrained, Some(requirement_source))
            }
            None => ConstrainedWithSource::new(
                Constrained::allow_any(default_permission_profile),
                /*source*/ None,
            ),
        };
        let windows_sandbox_mode = match windows {
            Some(Sourced {
                value:
                    WindowsRequirementsToml {
                        allowed_sandbox_implementations: Some(implementations),
                    },
                source: requirement_source,
            }) => {
                if implementations.is_empty() {
                    return Err(ConstraintError::empty_field(
                        "windows.allowed_sandbox_implementations",
                    ));
                }
                // Prefer elevated when both Windows sandbox implementations are allowed.
                let initial_value = if implementations.contains(&WindowsSandboxModeToml::Elevated) {
                    WindowsSandboxModeToml::Elevated
                } else {
                    WindowsSandboxModeToml::Unelevated
                };

                let requirement_source_for_error = requirement_source.clone();
                let constrained =
                    Constrained::new(Some(initial_value), move |candidate| match candidate {
                        Some(candidate) if implementations.contains(candidate) => Ok(()),
                        _ => Err(ConstraintError::InvalidValue {
                            field_name: "windows.sandbox",
                            candidate: format!("{candidate:?}"),
                            allowed: format!("{implementations:?}"),
                            requirement_source: requirement_source_for_error.clone(),
                        }),
                    })?;
                ConstrainedWithSource::new(constrained, Some(requirement_source))
            }
            Some(_) | None => ConstrainedWithSource::new(
                Constrained::allow_any(/*initial_value*/ None),
                /*source*/ None,
            ),
        };
        let exec_policy = match rules {
            Some(Sourced { value, source }) => {
                let policy = value.to_requirements_policy().map_err(|err| {
                    ConstraintError::ExecPolicyParse {
                        requirement_source: source.clone(),
                        reason: err.to_string(),
                    }
                })?;
                Some(Sourced::new(policy, source))
            }
            None => None,
        };
        let web_search_mode = match allowed_web_search_modes {
            Some(Sourced {
                value: modes,
                source: requirement_source,
            }) => {
                let mut accepted = modes.into_iter().collect::<std::collections::BTreeSet<_>>();
                accepted.insert(WebSearchModeRequirement::Disabled);
                let allowed_for_error = format!(
                    "{:?}",
                    accepted
                        .iter()
                        .copied()
                        .map(WebSearchMode::from)
                        .collect::<Vec<_>>()
                );

                let initial_value = if accepted.contains(&WebSearchModeRequirement::Cached) {
                    WebSearchMode::Cached
                } else if accepted.contains(&WebSearchModeRequirement::Indexed) {
                    WebSearchMode::Indexed
                } else if accepted.contains(&WebSearchModeRequirement::Live) {
                    WebSearchMode::Live
                } else {
                    WebSearchMode::Disabled
                };
                let requirement_source_for_error = requirement_source.clone();
                let constrained = Constrained::new(initial_value, move |candidate| {
                    if accepted.contains(&(*candidate).into()) {
                        Ok(())
                    } else {
                        Err(ConstraintError::InvalidValue {
                            field_name: "web_search_mode",
                            candidate: format!("{candidate:?}"),
                            allowed: allowed_for_error.clone(),
                            requirement_source: requirement_source_for_error.clone(),
                        })
                    }
                })?;
                ConstrainedWithSource::new(constrained, Some(requirement_source))
            }
            None => ConstrainedWithSource::new(
                Constrained::allow_any(WebSearchMode::Cached),
                /*source*/ None,
            ),
        };
        let feature_requirements =
            feature_requirements.filter(|requirements| !requirements.value.is_empty());
        let managed_hooks = hooks
            .filter(|managed_hooks| managed_hooks.value.handler_count() > 0)
            .map(|sourced_hooks| {
                let Sourced {
                    value,
                    source: requirement_source,
                } = sourced_hooks;
                let allowed = value;
                let allowed_for_error = format!("{allowed:?}");
                let requirement_source_for_error = requirement_source.clone();
                let constrained = Constrained::new(allowed.clone(), move |candidate| {
                    if candidate == &allowed {
                        Ok(())
                    } else {
                        Err(ConstraintError::InvalidValue {
                            field_name: "hooks",
                            candidate: format!("{candidate:?}"),
                            allowed: allowed_for_error.clone(),
                            requirement_source: requirement_source_for_error.clone(),
                        })
                    }
                })?;
                Ok(ConstrainedWithSource::new(
                    constrained,
                    Some(requirement_source),
                ))
            })
            .transpose()?;

        let enforce_residency = match enforce_residency {
            Some(Sourced {
                value: residency,
                source: requirement_source,
            }) => {
                let required = Some(residency);
                let requirement_source_for_error = requirement_source.clone();
                let constrained = Constrained::new(required, move |candidate| {
                    if candidate == &required {
                        Ok(())
                    } else {
                        Err(ConstraintError::InvalidValue {
                            field_name: "enforce_residency",
                            candidate: format!("{candidate:?}"),
                            allowed: format!("{required:?}"),
                            requirement_source: requirement_source_for_error.clone(),
                        })
                    }
                })?;
                ConstrainedWithSource::new(constrained, Some(requirement_source))
            }
            None => ConstrainedWithSource::new(
                Constrained::allow_any(/*initial_value*/ None),
                /*source*/ None,
            ),
        };
        let network = network.map(|sourced_network| {
            let Sourced { value, source } = sourced_network;
            Sourced::new(NetworkConstraints::from(value), source)
        });
        let filesystem = permissions.map(|sourced_permissions| {
            let Sourced { value, source } = sourced_permissions;
            Sourced::new(FilesystemConstraints::from(value), source)
        });
        Ok(ConfigRequirements {
            approval_policy,
            permission_profile,
            windows_sandbox_mode,
            web_search_mode,
            allow_managed_hooks_only,
            allow_appshots,
            allow_remote_control,
            computer_use,
            feature_requirements,
            managed_hooks,
            mcp_servers,
            plugins,
            marketplaces,
            exec_policy,
            enforce_residency,
            network,
            filesystem,
        })
    }
}

pub fn sandbox_mode_requirement_for_permission_profile(
    permission_profile: &PermissionProfile,
) -> SandboxModeRequirement {
    match permission_profile {
        PermissionProfile::Disabled => SandboxModeRequirement::DangerFullAccess,
        PermissionProfile::External { .. } => SandboxModeRequirement::ExternalSandbox,
        PermissionProfile::Managed { .. } => {
            let file_system_policy = permission_profile.file_system_sandbox_policy();
            if file_system_policy.has_full_disk_write_access() {
                SandboxModeRequirement::DangerFullAccess
            } else if file_system_policy
                .entries
                .iter()
                .any(|entry| entry.access.can_write())
            {
                SandboxModeRequirement::WorkspaceWrite
            } else {
                SandboxModeRequirement::ReadOnly
            }
        }
    }
}
