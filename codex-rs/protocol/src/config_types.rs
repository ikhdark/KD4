use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;
use strum_macros::Display;
use strum_macros::EnumIter;
use ts_rs::TS;
use wildmatch::WildMatchPattern;

use crate::openai_models::ReasoningEffort;

/// Terminal-title items used when no custom selection is configured.
pub const DEFAULT_TERMINAL_TITLE_ITEMS: &[&str] = &["activity", "project-name"];

/// HTTP payload encoding used by OTLP/HTTP exporters.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OtelHttpProtocol {
    Binary,
    Json,
}

/// TLS material used by OTLP exporters.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct OtelTlsConfig {
    pub ca_certificate: Option<AbsolutePathBuf>,
    pub client_certificate: Option<AbsolutePathBuf>,
    pub client_private_key: Option<AbsolutePathBuf>,
}

/// Selects which part of the active context is charged against
/// `model_auto_compact_token_limit`.
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutoCompactTokenLimitScope {
    /// Count the full active context against the limit.
    #[default]
    Total,
    /// Count sampled output and later growth after the carried window prefix.
    BodyAfterPrefix,
}

/// A summary of the reasoning performed by the model. This can be useful for
/// debugging and understanding the model's reasoning process.
/// See https://platform.openai.com/docs/guides/reasoning?api-mode=responses#reasoning-summaries
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ReasoningSummary {
    #[default]
    Auto,
    Concise,
    Detailed,
    /// Option to disable reasoning summaries.
    None,
}

/// Controls output length/detail on GPT-5 models via the Responses API.
/// Serialized with lowercase values to match the OpenAI API.
#[derive(
    Hash,
    Debug,
    Serialize,
    Deserialize,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Verbosity {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(
    Deserialize, Debug, Clone, Copy, PartialEq, Default, Serialize, Display, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum SandboxMode {
    #[serde(rename = "read-only")]
    #[default]
    ReadOnly,

    #[serde(rename = "workspace-write")]
    WorkspaceWrite,

    #[serde(rename = "danger-full-access")]
    DangerFullAccess,
}

/// Validated plain profile-v2 name used to select `$CODEX_HOME/<name>.config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileV2Name(String);

impl ProfileV2Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProfileV2NameParseError {
    value: String,
}

impl fmt::Display for ProfileV2NameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid --profile value `{}`; pass a plain name such as `work`",
            self.value
        )
    }
}

impl std::error::Error for ProfileV2NameParseError {}

impl FromStr for ProfileV2Name {
    type Err = ProfileV2NameParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ProfileV2NameParseError {
                value: value.to_string(),
            });
        }

        Ok(Self(value.to_string()))
    }
}

impl Deref for ProfileV2Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for ProfileV2Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ShellEnvironmentPolicyInherit {
    /// "Core" environment variables for the platform. On UNIX, this would
    /// include HOME, LOGNAME, PATH, SHELL, and USER, among others.
    Core,

    /// Inherits the full environment from the parent process.
    #[default]
    All,

    /// Do not inherit any environment variables from the parent process.
    None,
}

pub type EnvironmentVariablePattern = WildMatchPattern<'*', '?'>;

/// Deriving the `env` based on this policy works as follows:
/// 1. Create an initial map based on the `inherit` policy.
/// 2. If `ignore_default_excludes` is false, filter the map using the default
///    exclude pattern(s), which are: `"*KEY*"`, `"*SECRET*"`, and `"*TOKEN*"`.
/// 3. If `exclude` is not empty, filter the map using the provided patterns.
/// 4. Insert any entries from `r#set` into the map.
/// 5. If non-empty, filter the map using the `include_only` patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellEnvironmentPolicy {
    /// Starting point when building the environment.
    pub inherit: ShellEnvironmentPolicyInherit,

    /// True to skip the check to exclude default environment variables that
    /// contain "KEY", "SECRET", or "TOKEN" in their name. Defaults to true.
    pub ignore_default_excludes: bool,

    /// Environment variable names to exclude from the environment.
    pub exclude: Vec<EnvironmentVariablePattern>,

    /// (key, value) pairs to insert in the environment.
    pub r#set: HashMap<String, String>,

    /// Environment variable names to retain in the environment.
    pub include_only: Vec<EnvironmentVariablePattern>,

    /// If true, the shell profile will be used to run the command.
    pub use_profile: bool,
}

impl Default for ShellEnvironmentPolicy {
    fn default() -> Self {
        Self {
            inherit: ShellEnvironmentPolicyInherit::All,
            ignore_default_excludes: true,
            exclude: Vec::new(),
            r#set: HashMap::new(),
            include_only: Vec::new(),
            use_profile: false,
        }
    }
}

#[derive(
    Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Display, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum WindowsSandboxLevel {
    #[default]
    Disabled,
    RestrictedToken,
    Elevated,
}

/// Windows sandbox implementation selected for an explicit setup request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSandboxSetupMode {
    Elevated,
    Unelevated,
}

#[derive(
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
    PartialOrd,
    Ord,
    EnumIter,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Personality {
    None,
    Friendly,
    Pragmatic,
}

/// Controls the effective multi-agent delegation instructions for a turn. `custom` means the
/// configured mode hint defines the policy instead of a built-in policy.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Display, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase", from = "MultiAgentModeWire")]
#[ts(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum MultiAgentMode {
    Custom(String),
    #[default]
    ExplicitRequestOnly,
    Proactive,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum MultiAgentModeWire {
    None,
    Custom(String),
    ExplicitRequestOnly,
    Proactive,
}

impl From<MultiAgentModeWire> for MultiAgentMode {
    fn from(value: MultiAgentModeWire) -> Self {
        match value {
            MultiAgentModeWire::None => Self::Custom(String::new()),
            MultiAgentModeWire::Custom(hint_text) => Self::Custom(hint_text),
            MultiAgentModeWire::ExplicitRequestOnly => Self::ExplicitRequestOnly,
            MultiAgentModeWire::Proactive => Self::Proactive,
        }
    }
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WebSearchMode {
    Disabled,
    #[default]
    Cached,
    Indexed,
    Live,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchLocation {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

impl WebSearchLocation {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            country: other.country.clone().or_else(|| self.country.clone()),
            region: other.region.clone().or_else(|| self.region.clone()),
            city: other.city.clone().or_else(|| self.city.clone()),
            timezone: other.timezone.clone().or_else(|| self.timezone.clone()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchToolConfig {
    pub context_size: Option<WebSearchContextSize>,
    pub allowed_domains: Option<Vec<String>>,
    pub location: Option<WebSearchLocation>,
}

impl WebSearchToolConfig {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            context_size: other.context_size.or(self.context_size),
            allowed_domains: other
                .allowed_domains
                .clone()
                .or_else(|| self.allowed_domains.clone()),
            location: match (&self.location, &other.location) {
                (Some(location), Some(other_location)) => Some(location.merge(other_location)),
                (Some(location), None) => Some(location.clone()),
                (None, Some(other_location)) => Some(other_location.clone()),
                (None, None) => None,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchFilters {
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchUserLocationType {
    #[default]
    Approximate,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default)]
    pub r#type: WebSearchUserLocationType,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchConfig {
    pub filters: Option<WebSearchFilters>,
    pub user_location: Option<WebSearchUserLocation>,
    pub search_context_size: Option<WebSearchContextSize>,
}

impl From<WebSearchLocation> for WebSearchUserLocation {
    fn from(location: WebSearchLocation) -> Self {
        Self {
            r#type: WebSearchUserLocationType::Approximate,
            country: location.country,
            region: location.region,
            city: location.city,
            timezone: location.timezone,
        }
    }
}

impl From<WebSearchToolConfig> for WebSearchConfig {
    fn from(config: WebSearchToolConfig) -> Self {
        Self {
            filters: config
                .allowed_domains
                .map(|allowed_domains| WebSearchFilters {
                    allowed_domains: Some(allowed_domains),
                }),
            user_location: config.location.map(Into::into),
            search_context_size: config.context_size,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

/// Request/config sentinel for explicit standard routing.
///
/// This is not a catalog service tier id. It means the user intentionally
/// selected no service tier, so model catalog defaults should not apply.
pub const SERVICE_TIER_DEFAULT_REQUEST_VALUE: &str = "default";

impl ServiceTier {
    pub const fn request_value(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }

    pub fn from_request_value(value: &str) -> Option<Self> {
        match value {
            "fast" | "priority" => Some(Self::Fast),
            "flex" => Some(Self::Flex),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ForcedLoginMethod {
    Chatgpt,
    Api,
}

const DEFAULT_PROVIDER_AUTH_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS: u64 = 300_000;

/// Configuration for obtaining a provider bearer token from a command.
#[derive(Debug, Clone, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderAuthInfo {
    /// Command to execute. Bare names are resolved via `PATH`; paths are resolved against `cwd`.
    pub command: String,

    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,

    /// Maximum time to wait for the token command to exit successfully.
    #[serde(default = "default_provider_auth_timeout_ms")]
    pub timeout_ms: NonZeroU64,

    /// Maximum age for the cached token before rerunning the command.
    /// Set to `0` to disable proactive refresh and only rerun after a 401 retry path.
    #[serde(default = "default_provider_auth_refresh_interval_ms")]
    pub refresh_interval_ms: u64,

    /// Working directory used when running the token command.
    // The omitted value depends on the deserialization context, not the schema environment.
    #[schemars(
        default = "provider_auth_cwd_schema_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub cwd: AbsolutePathBuf,
}

impl<'de> Deserialize<'de> for ModelProviderAuthInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default = "default_provider_auth_timeout_ms")]
            timeout_ms: NonZeroU64,
            #[serde(default = "default_provider_auth_refresh_interval_ms")]
            refresh_interval_ms: u64,
            #[serde(default, deserialize_with = "deserialize_explicit_provider_auth_cwd")]
            cwd: Option<AbsolutePathBuf>,
        }

        let fields = Fields::deserialize(deserializer)?;
        let cwd = match fields.cwd {
            Some(cwd) => cwd,
            None => default_provider_auth_cwd().map_err(serde::de::Error::custom)?,
        };
        Ok(Self {
            command: fields.command,
            args: fields.args,
            timeout_ms: fields.timeout_ms,
            refresh_interval_ms: fields.refresh_interval_ms,
            cwd,
        })
    }
}

fn deserialize_explicit_provider_auth_cwd<'de, D>(
    deserializer: D,
) -> Result<Option<AbsolutePathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Only an absent field uses the default; an explicit null is still invalid.
    AbsolutePathBuf::deserialize(deserializer).map(Some)
}

impl ModelProviderAuthInfo {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        NonZeroU64::new(self.refresh_interval_ms).map(|value| Duration::from_millis(value.get()))
    }
}

fn default_provider_auth_timeout_ms() -> NonZeroU64 {
    non_zero_u64(
        DEFAULT_PROVIDER_AUTH_TIMEOUT_MS,
        "model_providers.<id>.auth.timeout_ms",
    )
}

fn default_provider_auth_refresh_interval_ms() -> u64 {
    DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS
}

fn non_zero_u64(value: u64, field_name: &str) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(value) => value,
        None => panic!("{field_name} must be non-zero"),
    }
}

fn default_provider_auth_cwd() -> std::io::Result<AbsolutePathBuf> {
    let deserializer = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(".");
    if let Ok(cwd) = AbsolutePathBuf::deserialize(deserializer) {
        return Ok(cwd);
    }

    AbsolutePathBuf::current_dir()
}

fn provider_auth_cwd_schema_default() -> Option<AbsolutePathBuf> {
    None
}

/// Represents the trust level for a project directory.
/// This determines the approval policy and sandbox mode applied.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// Controls whether the TUI uses the terminal's alternate screen buffer.
///
/// - `auto` (default): Use alternate screen mode.
/// - `always`: Always use alternate screen mode.
/// - `never`: Never use alternate screen mode. Runs in inline mode, preserving scrollback.
///
/// The CLI flag `--no-alt-screen` can override this setting at runtime.
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum AltScreenMode {
    /// Use alternate screen mode.
    #[default]
    Auto,
    /// Always use alternate screen mode.
    Always,
    /// Never use alternate screen (inline mode only).
    Never,
}

/// Initial collaboration mode to use when the TUI starts.
#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Plan,
    #[default]
    #[serde(
        alias = "code",
        alias = "pair_programming",
        alias = "execute",
        alias = "custom"
    )]
    Default,
    #[doc(hidden)]
    #[serde(skip_serializing, skip_deserializing)]
    #[schemars(skip)]
    #[ts(skip)]
    PairProgramming,
    #[doc(hidden)]
    #[serde(skip_serializing, skip_deserializing)]
    #[schemars(skip)]
    #[ts(skip)]
    Execute,
}

pub const TUI_VISIBLE_COLLABORATION_MODES: [ModeKind; 2] = [ModeKind::Default, ModeKind::Plan];

impl ModeKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Default => "Default",
            Self::PairProgramming => "Pair Programming",
            Self::Execute => "Execute",
        }
    }

    pub const fn is_tui_visible(self) -> bool {
        matches!(self, Self::Plan | Self::Default)
    }

    pub const fn allows_request_user_input(self) -> bool {
        matches!(self, Self::Plan)
    }
}

/// Collaboration mode for a Codex session.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub struct CollaborationMode {
    pub mode: ModeKind,
    pub settings: Settings,
}

impl CollaborationMode {
    /// Returns a reference to the settings.
    fn settings_ref(&self) -> &Settings {
        &self.settings
    }

    pub fn model(&self) -> &str {
        self.settings_ref().model.as_str()
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.settings_ref().reasoning_effort.clone()
    }

    /// Updates the collaboration mode with new model and/or effort values.
    ///
    /// - `model`: `Some(s)` to update the model, `None` to keep the current model
    /// - `effort`: `Some(Some(e))` to set effort to `e`, `Some(None)` to clear effort, `None` to keep current effort
    /// - `developer_instructions`: `Some(Some(s))` to set instructions, `Some(None)` to clear them, `None` to keep current
    ///
    /// Returns a new `CollaborationMode` with updated values, preserving the mode.
    pub fn with_updates(
        &self,
        model: Option<String>,
        effort: Option<Option<ReasoningEffort>>,
        developer_instructions: Option<Option<String>>,
    ) -> Self {
        let settings = self.settings_ref();
        let updated_settings = Settings {
            model: model.unwrap_or_else(|| settings.model.clone()),
            reasoning_effort: effort.unwrap_or_else(|| settings.reasoning_effort.clone()),
            developer_instructions: developer_instructions
                .unwrap_or_else(|| settings.developer_instructions.clone()),
        };

        CollaborationMode {
            mode: self.mode,
            settings: updated_settings,
        }
    }

    /// Applies a mask to this collaboration mode, returning a new collaboration mode
    /// with the mask values applied. Fields in the mask that are `Some` will override
    /// the corresponding fields, while `None` values will preserve the original values.
    ///
    /// The `name` field in the mask is ignored as it's metadata for the mask itself.
    pub fn apply_mask(&self, mask: &CollaborationModeMask) -> Self {
        let settings = self.settings_ref();
        CollaborationMode {
            mode: mask.mode.unwrap_or(self.mode),
            settings: Settings {
                model: mask.model.clone().unwrap_or_else(|| settings.model.clone()),
                reasoning_effort: mask
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| settings.reasoning_effort.clone()),
                developer_instructions: mask
                    .developer_instructions
                    .clone()
                    .unwrap_or_else(|| settings.developer_instructions.clone()),
            },
        }
    }
}

/// Settings for a collaboration mode.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct Settings {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub developer_instructions: Option<String>,
}

/// A mask for collaboration mode settings, allowing partial updates.
/// All fields except `name` are optional, enabling selective updates.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct CollaborationModeMask {
    pub name: String,
    pub mode: Option<ModeKind>,
    pub model: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "serde_with::rust::double_option::deserialize"
    )]
    #[ts(optional = nullable)]
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "serde_with::rust::double_option::deserialize"
    )]
    #[ts(optional = nullable)]
    pub developer_instructions: Option<Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn provider_auth_defaults_use_scoped_directory() {
        let directory = tempfile::tempdir().expect("create directory");
        let _guard = codex_utils_absolute_path::AbsolutePathBufGuard::new(directory.path());
        let auth: ModelProviderAuthInfo =
            serde_json::from_value(serde_json::json!({ "command": "token-helper" }))
                .expect("deserialize provider auth");

        assert_eq!(
            auth,
            ModelProviderAuthInfo {
                command: "token-helper".to_owned(),
                args: Vec::new(),
                timeout_ms: NonZeroU64::new(5_000).expect("nonzero timeout"),
                refresh_interval_ms: 300_000,
                cwd: AbsolutePathBuf::try_from(directory.path()).expect("absolute directory"),
            }
        );
    }

    #[test]
    fn provider_auth_defaults_use_current_directory_without_scope() {
        let auth: ModelProviderAuthInfo =
            serde_json::from_value(serde_json::json!({ "command": "token-helper" }))
                .expect("deserialize provider auth");
        assert_eq!(
            auth.cwd,
            AbsolutePathBuf::current_dir().expect("current directory")
        );
    }

    #[test]
    fn provider_auth_explicit_values_preserve_validation() {
        let directory = tempfile::tempdir().expect("create directory");
        let value = serde_json::json!({
            "command": "token-helper",
            "args": ["--token"],
            "timeout_ms": 7,
            "refresh_interval_ms": 0,
            "cwd": directory.path(),
        });
        let auth: ModelProviderAuthInfo =
            serde_json::from_value(value.clone()).expect("deserialize explicit values");
        assert_eq!(serde_json::to_value(auth).expect("serialize auth"), value);

        for invalid in [
            serde_json::json!({ "command": "token-helper", "cwd": null }),
            serde_json::json!({ "command": "token-helper", "cwd": "relative" }),
            serde_json::json!({ "command": "token-helper", "timeout_ms": 0 }),
        ] {
            assert!(
                serde_json::from_value::<ModelProviderAuthInfo>(invalid.clone()).is_err(),
                "invalid provider auth was accepted: {invalid}"
            );
        }
    }

    #[test]
    fn provider_auth_schema_preserves_optional_cwd_without_environment_default() {
        let schema = serde_json::to_value(schemars::schema_for!(ModelProviderAuthInfo))
            .expect("serialize schema");
        assert_eq!(schema["required"], serde_json::json!(["command"]));
        let cwd = &schema["properties"]["cwd"];
        assert!(cwd.is_object(), "cwd must remain in the schema");
        assert!(cwd.get("default").is_none());
        assert_eq!(
            cwd["allOf"],
            serde_json::json!([{ "$ref": "#/definitions/AbsolutePathBuf" }])
        );
    }

    #[cfg(unix)]
    #[test]
    fn provider_auth_missing_current_directory_returns_deserialization_error() {
        const CHILD_MARKER: &str = "CODEX_TEST_PROVIDER_AUTH_MISSING_CWD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            // Isolate the unavailable process cwd from other tests in this process.
            std::fs::remove_dir(std::env::current_dir().expect("initial child directory"))
                .expect("remove child directory");
            let result = serde_json::from_value::<ModelProviderAuthInfo>(
                serde_json::json!({ "command": "token-helper" }),
            );
            assert!(result.is_err(), "unavailable cwd must be a serde error");
            return;
        }

        let directory = tempfile::tempdir().expect("create parent directory");
        let child_directory = directory.path().join("cwd");
        std::fs::create_dir(&child_directory).expect("create child directory");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "config_types::tests::provider_auth_missing_current_directory_returns_deserialization_error",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .current_dir(child_directory)
            .output()
            .expect("run child test");
        assert!(
            output.status.success(),
            "child test failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn apply_mask_preserves_missing_null_and_value_through_json() {
        let mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: "gpt-5.2-codex".to_string(),
                reasoning_effort: Some(ReasoningEffort::High),
                developer_instructions: Some("stay focused".to_string()),
            },
        };
        for (fields, effort, instructions) in [
            (
                serde_json::json!({}),
                Some(ReasoningEffort::High),
                Some("stay focused"),
            ),
            (
                serde_json::json!({"reasoning_effort": null, "developer_instructions": null}),
                None,
                None,
            ),
            (
                serde_json::json!({"reasoning_effort": "low", "developer_instructions": "new instructions"}),
                Some(ReasoningEffort::Low),
                Some("new instructions"),
            ),
        ] {
            let mut json = serde_json::json!({"name": "Mask", "mode": null, "model": null});
            json.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let mask: CollaborationModeMask = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(serde_json::to_value(&mask).unwrap(), json);
            assert_eq!(
                mode.apply_mask(&mask),
                CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: "gpt-5.2-codex".to_string(),
                        reasoning_effort: effort,
                        developer_instructions: instructions.map(str::to_string),
                    },
                }
            );
        }
    }

    #[test]
    fn mode_kind_deserializes_alias_values_to_default() {
        for alias in ["code", "pair_programming", "execute", "custom"] {
            let json = format!("\"{alias}\"");
            let mode: ModeKind = serde_json::from_str(&json).expect("deserialize mode");
            assert_eq!(ModeKind::Default, mode);
        }
    }

    #[test]
    fn profile_v2_name_rejects_paths_and_empty_names() {
        assert_eq!(
            ProfileV2Name::from_str("../foo"),
            Err(ProfileV2NameParseError {
                value: "../foo".to_string(),
            }),
            "dots and slashes are disallowed to prevent reading arbitrary files"
        );
        assert_eq!(
            ProfileV2Name::from_str(""),
            Err(ProfileV2NameParseError {
                value: String::new(),
            }),
            "profile name cannot be empty"
        );
    }

    #[test]
    fn tui_visible_collaboration_modes_match_mode_kind_visibility() {
        let expected = [ModeKind::Default, ModeKind::Plan];
        assert_eq!(expected, TUI_VISIBLE_COLLABORATION_MODES);

        for mode in TUI_VISIBLE_COLLABORATION_MODES {
            assert!(mode.is_tui_visible());
        }

        assert!(!ModeKind::PairProgramming.is_tui_visible());
        assert!(!ModeKind::Execute.is_tui_visible());
    }

    #[test]
    fn web_search_location_merge_prefers_overlay_values() {
        let base = WebSearchLocation {
            country: Some("US".to_string()),
            region: Some("CA".to_string()),
            city: None,
            timezone: Some("America/Los_Angeles".to_string()),
        };
        let overlay = WebSearchLocation {
            country: None,
            region: Some("WA".to_string()),
            city: Some("Seattle".to_string()),
            timezone: None,
        };

        let expected = WebSearchLocation {
            country: Some("US".to_string()),
            region: Some("WA".to_string()),
            city: Some("Seattle".to_string()),
            timezone: Some("America/Los_Angeles".to_string()),
        };

        assert_eq!(expected, base.merge(&overlay));
    }

    #[test]
    fn web_search_tool_config_merge_prefers_overlay_values() {
        let base = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::Low),
            allowed_domains: Some(vec!["openai.com".to_string()]),
            location: Some(WebSearchLocation {
                country: Some("US".to_string()),
                region: Some("CA".to_string()),
                city: None,
                timezone: Some("America/Los_Angeles".to_string()),
            }),
        };
        let overlay = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::High),
            allowed_domains: None,
            location: Some(WebSearchLocation {
                country: None,
                region: Some("WA".to_string()),
                city: Some("Seattle".to_string()),
                timezone: None,
            }),
        };

        let expected = WebSearchToolConfig {
            context_size: Some(WebSearchContextSize::High),
            allowed_domains: Some(vec!["openai.com".to_string()]),
            location: Some(WebSearchLocation {
                country: Some("US".to_string()),
                region: Some("WA".to_string()),
                city: Some("Seattle".to_string()),
                timezone: Some("America/Los_Angeles".to_string()),
            }),
        };

        assert_eq!(expected, base.merge(&overlay));
    }
}
