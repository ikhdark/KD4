//! Schema-heavy configuration TOML types used by Codex.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use crate::HooksToml;
use crate::permissions_toml::PermissionsToml;
use crate::types::AnalyticsConfigToml;
use crate::types::AppsConfigToml;
use crate::types::AuthCredentialsStoreMode;
use crate::types::FeedbackConfigToml;
use crate::types::History;
use crate::types::MarketplaceConfig;
use crate::types::McpServerConfig;
use crate::types::Notice;
use crate::types::OAuthCredentialsStoreMode;
use crate::types::OtelConfigToml;
use crate::types::PluginConfig;
use crate::types::SandboxWorkspaceWrite;
use crate::types::ShellEnvironmentPolicyToml;
use crate::types::SkillsConfig;
use crate::types::ToolSuggestConfig;
use crate::types::Tui;
use crate::types::UriBasedFileOpener;
use crate::types::WindowsToml;
use codex_features::FeaturesToml;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::AMAZON_BEDROCK_RUNTIME_PROVIDER_ID;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WebSearchToolConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::validation::ValidationMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::normalize_for_native_workdir;
use codex_utils_absolute_path::normalize_for_path_comparison;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;
use serde_json::Value as JsonValue;

const RESERVED_MODEL_PROVIDER_IDS: [&str; 5] = [
    AMAZON_BEDROCK_PROVIDER_ID,
    AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
    OPENAI_PROVIDER_ID,
    OLLAMA_OSS_PROVIDER_ID,
    LMSTUDIO_OSS_PROVIDER_ID,
];

pub const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Converts a configured ChatGPT URL into the one internal backend base URL
/// used by all runtime consumers.
pub fn canonicalize_chatgpt_base_url(value: &str) -> String {
    let value = value.trim();
    let Ok(mut url) = url::Url::parse(value) else {
        return value.trim_end_matches('/').to_string();
    };

    let path = url.path().trim_end_matches('/');
    let is_backend_client_chatgpt_host =
        matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"));
    let is_known_chatgpt_alias = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("chatgpt.com" | "chat.openai.com" | "chatgpt-staging.com")
        )
        && matches!(path, "" | "/backend-api" | "/codex" | "/backend-api/codex");

    if is_known_chatgpt_alias {
        url.set_path("/backend-api");
    } else {
        let path = path.to_string();
        url.set_path(&path);
    }

    let mut canonical = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        canonical.pop();
    }
    if !is_backend_client_chatgpt_host
        && url.username().is_empty()
        && url.password().is_none()
        && (canonical.starts_with("https://chatgpt.com")
            || canonical.starts_with("https://chat.openai.com"))
    {
        // backend-client is an exact upstream mirror and currently recognizes the official
        // ChatGPT origins with a case-sensitive string prefix. Preserve a custom host's URL
        // semantics while ensuring a suffix such as `.example` cannot be mistaken for an
        // official origin and rewritten to `/backend-api` inside that protected crate.
        canonical.replace_range("https://".len().."https://".len() + 1, "C");
    }
    canonical
}

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

const fn default_allow_login_shell() -> Option<bool> {
    Some(true)
}

fn default_history() -> Option<History> {
    Some(History::default())
}

const fn default_project_doc_max_bytes() -> Option<usize> {
    Some(DEFAULT_PROJECT_DOC_MAX_BYTES)
}

fn default_project_doc_fallback_filenames() -> Option<Vec<String>> {
    Some(Vec::new())
}

const fn default_hide_agent_reasoning() -> Option<bool> {
    Some(false)
}

const fn default_true() -> bool {
    true
}

/// Backward-compatible shape for ChatGPT workspace login restrictions in config.toml.
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum ForcedChatgptWorkspaceIds {
    Single(String),
    Multiple(Vec<String>),
}

impl ForcedChatgptWorkspaceIds {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => vec![value],
            Self::Multiple(values) => values,
        }
    }
}

impl<'de> Deserialize<'de> for ForcedChatgptWorkspaceIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Single(String),
            Multiple(Vec<String>),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Single(value) if value.contains(',') => Err(D::Error::custom(
                "forced_chatgpt_workspace_id must be a single workspace ID string or a TOML list \
of strings; comma-separated strings are not supported. Use \
`forced_chatgpt_workspace_id = [\"123e4567-e89b-42d3-a456-426614174000\", \
\"123e4567-e89b-42d3-a456-426614174001\"]` instead.",
            )),
            Repr::Single(value) => Ok(Self::Single(value)),
            Repr::Multiple(values) => Ok(Self::Multiple(values)),
        }
    }
}

/// Orchestrator-owned feature settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorToml {
    pub skills: Option<OrchestratorFeatureToml>,
    pub mcp: Option<OrchestratorFeatureToml>,
}

/// Settings for a feature owned by the orchestrator.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorFeatureToml {
    pub enabled: Option<bool>,
}

pub const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(transparent)]
pub struct ConfigVersion(pub u32);

impl Default for ConfigVersion {
    fn default() -> Self {
        Self(CURRENT_CONFIG_VERSION)
    }
}

/// Ordering policy for the AfterAgent hook at turn completion.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AfterAgentPolicy {
    #[default]
    Legacy,
    MutatingFinalizer,
}

/// Base config deserialized from ~/.codex/config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigToml {
    /// Version of the persisted configuration shape. Unversioned files are
    /// upgraded by the loader before this structure is deserialized.
    #[serde(default)]
    pub config_version: ConfigVersion,

    /// Optional override of model selection.
    pub model: Option<String>,
    /// Review model override used by the `/review` feature.
    pub review_model: Option<String>,

    /// Provider to use from the model_providers map.
    pub model_provider: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether the auto-compaction limit applies to the full context or
    /// only to tokens after the carried prefix in the current compaction window.
    pub model_auto_compact_token_limit_scope: Option<AutoCompactTokenLimitScope>,

    /// Default approval policy for executing commands.
    pub approval_policy: Option<AskForApproval>,

    #[serde(default)]
    pub shell_environment_policy: ShellEnvironmentPolicyToml,

    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    #[serde(default = "default_allow_login_shell")]
    pub allow_login_shell: Option<bool>,

    /// Sandbox mode to use.
    pub sandbox_mode: Option<SandboxMode>,

    /// Sandbox configuration to apply if `sandbox` is `WorkspaceWrite`.
    pub sandbox_workspace_write: Option<SandboxWorkspaceWrite>,

    /// Default permissions profile to apply. Names starting with `:` refer to
    /// built-in profiles; other names are resolved from the `[permissions]`
    /// table.
    pub default_permissions: Option<String>,

    /// Named permissions profiles.
    #[serde(default)]
    pub permissions: Option<PermissionsToml>,

    /// Optional external command to spawn for end-user notifications.
    #[serde(default)]
    pub notify: Option<Vec<String>>,

    /// Ordering policy for the AfterAgent hook at turn completion.
    #[serde(default)]
    pub after_agent_policy: AfterAgentPolicy,

    /// System instructions.
    pub instructions: Option<String>,

    /// Developer instructions inserted as a `developer` role message.
    #[serde(default)]
    pub developer_instructions: Option<String>,

    /// Whether to inject the `<permissions instructions>` developer block.
    pub include_permissions_instructions: Option<bool>,

    /// Whether to inject the `<apps_instructions>` developer block.
    pub include_apps_instructions: Option<bool>,

    /// Whether to inject the `<collaboration_mode>` developer block.
    pub include_collaboration_mode_instructions: Option<bool>,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: Option<bool>,

    /// Optional path to a file containing model instructions that will override
    /// the built-in instructions for the selected model. Users are STRONGLY
    /// DISCOURAGED from using this field, as deviating from the instructions
    /// sanctioned by Codex will likely degrade model performance.
    pub model_instructions_file: Option<AbsolutePathBuf>,

    /// Compact prompt used for history compaction.
    pub compact_prompt: Option<String>,

    /// When set, restricts ChatGPT login to one or more workspace identifiers.
    #[serde(default)]
    pub forced_chatgpt_workspace_id: Option<ForcedChatgptWorkspaceIds>,

    /// When set, restricts the login mechanism users may use.
    #[serde(default)]
    pub forced_login_method: Option<ForcedLoginMethod>,

    /// Preferred backend for storing CLI auth credentials.
    /// file (default): Use a file in the Codex home directory.
    /// keyring: Use an OS-specific keyring service.
    /// auto: Use the keyring if available, otherwise use a file.
    #[serde(default)]
    pub cli_auth_credentials_store: Option<AuthCredentialsStoreMode>,

    /// Definition for MCP servers that Codex can reach out to for tool calls.
    #[serde(default)]
    // Uses the raw MCP input shape (custom deserialization) rather than `McpServerConfig`.
    #[schemars(schema_with = "crate::schema::mcp_servers_schema")]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Preferred backend for storing MCP OAuth credentials.
    /// keyring: Use an OS-specific keyring service.
    ///          https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/oauth.rs#L2
    /// file: Use a file in the Codex home directory.
    /// auto (default): Use the OS-specific keyring service if available, otherwise use a file.
    #[serde(default)]
    pub mcp_oauth_credentials_store: Option<OAuthCredentialsStoreMode>,

    /// Optional fixed port for the local HTTP callback server used during MCP OAuth login.
    /// When unset, Codex will bind to an ephemeral port chosen by the OS.
    pub mcp_oauth_callback_port: Option<u16>,

    /// Optional redirect URI to use during MCP OAuth login.
    /// When set, this URI is used in the OAuth authorization request instead
    /// of the local listener address. The local callback listener still binds
    /// to 127.0.0.1 (using `mcp_oauth_callback_port` when provided).
    pub mcp_oauth_callback_url: Option<String>,

    /// User-defined provider entries that extend the built-in list. Built-in
    /// IDs cannot be overridden.
    #[serde(default, deserialize_with = "deserialize_model_providers")]
    pub model_providers: HashMap<String, ModelProviderInfo>,

    /// Maximum combined source bytes to retain from AGENTS.md project docs.
    /// The rendered project-doc payload, including generated provenance and
    /// notices, is separately capped at this value plus 4 KiB.
    #[serde(default = "default_project_doc_max_bytes")]
    pub project_doc_max_bytes: Option<usize>,

    /// Ordered list of fallback filenames to look for when AGENTS.md is missing.
    #[serde(default = "default_project_doc_fallback_filenames")]
    pub project_doc_fallback_filenames: Option<Vec<String>>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Default validation execution policy. Defaults to `full.5` (Full .5): compile checks and other operations are allowed, but tests/full test suites are not. Non-compilation checks are prohibited except in permitted non-test KD4 scripts workflows. Legacy `benchmark` settings remain accepted.
    pub validation_mode: Option<ValidationMode>,

    /// Whether completed textual tool outputs may be represented by bounded,
    /// exactly retrievable receipts in manually managed prompt history.
    pub completed_tool_history_projection: Option<bool>,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: Option<u64>,

    /// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
    #[serde(default = "default_history")]
    pub history: Option<History>,

    /// Directory where Codex stores the SQLite state DB.
    /// Defaults to `$CODEX_SQLITE_HOME` when set. Otherwise uses `$CODEX_HOME`.
    pub sqlite_home: Option<AbsolutePathBuf>,

    /// Directory where Codex writes log files. Setting this value explicitly
    /// also enables the TUI text log in this directory.
    /// Defaults to `$CODEX_HOME/log`.
    pub log_dir: Option<AbsolutePathBuf>,

    /// Debugging and reproducibility settings.
    pub debug: Option<DebugToml>,

    /// Optional URI-based file opener. If set, local Markdown links in model
    /// output will be hyperlinked using the specified URI scheme.
    pub file_opener: Option<UriBasedFileOpener>,

    /// Collection of settings that are specific to the TUI.
    pub tui: Option<Tui>,

    /// When set to `true`, `AgentReasoning` events will be hidden from the
    /// UI/output. Defaults to `false`.
    #[serde(default = "default_hide_agent_reasoning")]
    pub hide_agent_reasoning: Option<bool>,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: Option<bool>,

    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Optional path to a JSON model catalog (applied on startup only).
    /// Per-thread `config` overrides are accepted but do not reapply this (no-ops).
    pub model_catalog_json: Option<AbsolutePathBuf>,

    /// Optionally specify a personality for the model
    pub personality: Option<Personality>,

    /// Optional explicit service tier request id for new turns (for example
    /// `default`, `priority`, or `flex`).
    pub service_tier: Option<String>,

    /// Base URL for requests to ChatGPT (as opposed to the OpenAI API).
    pub chatgpt_base_url: Option<String>,

    /// Optional product SKU forwarded on host-owned Codex Apps MCP requests.
    pub apps_mcp_product_sku: Option<String>,

    /// Orchestrator-owned feature settings.
    pub orchestrator: Option<OrchestratorToml>,

    /// Base URL override for the built-in `openai` model provider.
    pub openai_base_url: Option<String>,

    /// Experimental / do not use. When set, app-server fetches thread-scoped
    /// config from a remote service at this endpoint.
    pub experimental_thread_config_endpoint: Option<String>,

    /// Removed. Former remote thread-store endpoint setting kept only so we can
    /// fail fast instead of silently falling back to local persistence.
    #[schemars(skip)]
    pub experimental_thread_store_endpoint: Option<String>,

    /// Experimental / do not use. Selects the thread store implementation.
    pub experimental_thread_store: Option<ThreadStoreToml>,
    pub projects: Option<HashMap<String, ProjectConfig>>,

    /// Controls the web search tool mode: disabled, cached, indexed, or live.
    pub web_search: Option<WebSearchMode>,

    /// Nested tools section for feature toggles
    pub tools: Option<ToolsToml>,

    /// Additional discoverable tools that can be suggested for installation.
    pub tool_suggest: Option<ToolSuggestConfig>,

    /// Agent-related settings (thread limits, etc.).
    pub agents: Option<AgentsToml>,

    /// User-level skill config entries keyed by SKILL.md path.
    pub skills: Option<SkillsConfig>,

    /// Lifecycle hooks configured inline in TOML plus user-level overrides.
    pub hooks: Option<HooksToml>,

    /// User-level plugin config entries keyed by plugin name.
    #[serde(default)]
    pub plugins: HashMap<String, PluginConfig>,

    /// User-level marketplace entries keyed by marketplace name.
    #[serde(default)]
    pub marketplaces: HashMap<String, MarketplaceConfig>,

    /// Centralized feature flags (new). Prefer this over individual toggles.
    #[serde(default)]
    // Injects known feature keys into the schema and forbids unknown keys.
    #[schemars(schema_with = "crate::schema::features_schema")]
    pub features: Option<FeaturesToml>,

    /// Suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: Option<bool>,

    /// Markers used to detect the project root when searching parent
    /// directories for `.codex` folders. Defaults to [".git"] when unset.
    #[serde(default)]
    pub project_root_markers: Option<Vec<String>>,

    /// When `true`, checks for Codex updates on startup and surfaces update prompts.
    /// Set to `false` only if your Codex updates are centrally managed.
    /// Defaults to `true`.
    pub check_for_update_on_startup: Option<bool>,

    /// When true, disables burst-paste detection for typed input entirely.
    /// All characters are inserted as they are received, and no buffering
    /// or placeholder replacement will occur for fast keypress bursts.
    pub disable_paste_burst: Option<bool>,

    /// When `false`, disables analytics across Codex product surfaces in this machine.
    /// Defaults to `true`.
    pub analytics: Option<AnalyticsConfigToml>,

    /// When `false`, disables feedback collection across Codex product surfaces.
    /// Defaults to `true`.
    pub feedback: Option<FeedbackConfigToml>,

    /// Settings for app-specific controls.
    #[serde(default)]
    pub apps: Option<AppsConfigToml>,

    /// Opaque desktop settings stored alongside the rest of config.toml.
    #[serde(default)]
    pub desktop: Option<HashMap<String, JsonValue>>,

    /// OTEL configuration.
    pub otel: Option<OtelConfigToml>,

    /// Windows-specific configuration.
    #[serde(default)]
    pub windows: Option<WindowsToml>,

    /// Collection of in-product notices (different from notifications)
    /// See [`crate::types::Notice`] for more details
    pub notice: Option<Notice>,

    pub experimental_compact_prompt_file: Option<AbsolutePathBuf>,
    /// Preferred OSS provider for local models, e.g. "lmstudio" or "ollama".
    pub oss_provider: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigLockfileToml {
    pub version: u32,
    pub codex_version: String,

    /// Replayable effective config captured in the lockfile.
    pub config: ConfigToml,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DebugToml {
    pub config_lockfile: Option<DebugConfigLockToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DebugConfigLockToml {
    /// Directory where Codex writes effective session config lock files.
    pub export_dir: Option<AbsolutePathBuf>,

    /// Lockfile to replay as the authoritative effective config.
    pub load_path: Option<AbsolutePathBuf>,

    /// Allow replaying a lock generated by a different Codex version.
    pub allow_codex_version_mismatch: Option<bool>,

    /// Save fields resolved from the model catalog/session configuration.
    pub save_fields_resolved_from_model_catalog: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStoreToml {
    Local {},
    #[schemars(skip)]
    InMemory {
        id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProjectConfig {
    pub trust_level: Option<TrustLevel>,
}

impl ProjectConfig {
    pub fn is_trusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Trusted))
    }

    pub fn is_untrusted(&self) -> bool {
        matches!(self.trust_level, Some(TrustLevel::Untrusted))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolsToml {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_web_search_tool_config"
    )]
    pub web_search: Option<WebSearchToolConfig>,
    pub experimental_request_user_input: Option<ExperimentalRequestUserInput>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExperimentalRequestUserInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebSearchToolConfigInput {
    Enabled(bool),
    Config(WebSearchToolConfig),
}

fn deserialize_optional_web_search_tool_config<'de, D>(
    deserializer: D,
) -> Result<Option<WebSearchToolConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<WebSearchToolConfigInput>::deserialize(deserializer)?;

    match value {
        None => Ok(None),
        Some(WebSearchToolConfigInput::Enabled(enabled)) => {
            let _ = enabled;
            Err(<D::Error as serde::de::Error>::custom(
                "`tools.web_search` no longer accepts a boolean; set top-level \
                     `web_search = \"live\"` or `web_search = \"disabled\"`, and use \
                     `[tools.web_search]` only for search parameters",
            ))
        }
        Some(WebSearchToolConfigInput::Config(config)) => Ok(Some(config)),
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentsToml {
    /// Maximum concurrent non-root agent threads for the legacy runtime.
    /// When unset, the runtime default applies. Cannot be set when multi-agent V2 is enabled;
    /// configure `features.multi_agent_v2.max_concurrent_threads_per_session` instead.
    #[schemars(range(min = 1))]
    pub max_threads: Option<usize>,
    /// Legacy nesting-depth setting; root sessions start at depth 0.
    /// Multi-agent V2 does not apply this setting and uses runtime admission limits instead.
    #[schemars(range(min = 1))]
    pub max_depth: Option<i32>,
    /// Default maximum runtime in seconds for agent job workers.
    #[schemars(range(min = 1))]
    pub job_max_runtime_seconds: Option<u64>,
    /// Whether to record a model-visible message when an agent turn is interrupted.
    /// Defaults to true.
    pub interrupt_message: Option<bool>,

    /// User-defined role declarations keyed by role name.
    ///
    /// Example:
    /// ```toml
    /// [agents.researcher]
    /// description = "Research-focused role."
    /// config_file = "./agents/researcher.toml"
    /// nickname_candidates = ["Herodotus", "Ibn Battuta"]
    /// ```
    #[serde(default, flatten)]
    pub roles: BTreeMap<String, AgentRoleToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentRoleToml {
    /// Human-facing role documentation used in spawn tool guidance.
    /// Required unless supplied by the referenced agent role file.
    pub description: Option<String>,

    /// Path to a role-specific config layer.
    /// Relative paths are resolved relative to the `config.toml` that defines them.
    pub config_file: Option<AbsolutePathBuf>,

    /// Candidate nicknames for agents spawned with this role.
    pub nickname_candidates: Option<Vec<String>>,
}

fn is_unsandboxed_windows(windows_sandbox_level: WindowsSandboxLevel, is_windows: bool) -> bool {
    is_windows && windows_sandbox_level == WindowsSandboxLevel::Disabled
}

const fn current_platform_is_windows() -> bool {
    cfg!(windows)
}

impl ConfigToml {
    /// Derive the effective permission profile from legacy sandbox config.
    ///
    /// Call this only after ruling out `default_permissions`: named
    /// `[permissions]` profiles must be compiled through the permissions
    /// profile pipeline, not reconstructed from `sandbox_mode`.
    pub async fn derive_permission_profile(
        &self,
        sandbox_mode_override: Option<SandboxMode>,
        windows_sandbox_level: WindowsSandboxLevel,
        active_project: Option<&ProjectConfig>,
        permission_profile_constraint: Option<&crate::Constrained<PermissionProfile>>,
    ) -> PermissionProfile {
        let is_unsandboxed_windows =
            is_unsandboxed_windows(windows_sandbox_level, current_platform_is_windows());
        let configured_sandbox_mode = sandbox_mode_override.or(self.sandbox_mode);
        let resolved_sandbox_mode = configured_sandbox_mode
            .or_else(|| {
                // If no sandbox_mode is set but this directory has a trust decision,
                // default to workspace-write except on unsandboxed Windows where we
                // default to read-only.
                active_project
                    .filter(|project| project.is_trusted() || project.is_untrusted())
                    .map(|_| {
                        if is_unsandboxed_windows {
                            SandboxMode::ReadOnly
                        } else {
                            SandboxMode::WorkspaceWrite
                        }
                    })
            })
            .unwrap_or_default();
        let effective_sandbox_mode = if is_unsandboxed_windows
            // If the experimental Windows sandbox is enabled, do not force a downgrade.
            && matches!(resolved_sandbox_mode, SandboxMode::WorkspaceWrite)
        {
            SandboxMode::ReadOnly
        } else {
            resolved_sandbox_mode
        };

        let permission_profile = match effective_sandbox_mode {
            SandboxMode::ReadOnly => PermissionProfile::read_only(),
            SandboxMode::WorkspaceWrite => match self.sandbox_workspace_write.as_ref() {
                Some(SandboxWorkspaceWrite {
                    writable_roots,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                }) => {
                    let network_policy = if *network_access {
                        NetworkSandboxPolicy::Enabled
                    } else {
                        NetworkSandboxPolicy::Restricted
                    };
                    let writable_roots = writable_roots.clone();
                    let exclude_tmpdir_env_var = *exclude_tmpdir_env_var;
                    let exclude_slash_tmp = *exclude_slash_tmp;
                    tokio::task::spawn_blocking(move || {
                        PermissionProfile::workspace_write_with(
                            &writable_roots,
                            network_policy,
                            exclude_tmpdir_env_var,
                            exclude_slash_tmp,
                        )
                    })
                    .await
                    .unwrap_or_else(|error| {
                        tracing::error!(%error, "permission profile projection worker failed; falling back to read-only");
                        PermissionProfile::read_only()
                    })
                }
                None => PermissionProfile::workspace_write(),
            },
            SandboxMode::DangerFullAccess => PermissionProfile::Disabled,
        };
        if configured_sandbox_mode.is_none()
            && let Some(constraint) = permission_profile_constraint
            && let Err(err) = constraint.can_set(&permission_profile)
        {
            tracing::warn!(
                error = %err,
                "default sandbox policy is disallowed by requirements; falling back to required default"
            );
            PermissionProfile::read_only()
        } else {
            permission_profile
        }
    }

    /// Resolves the cwd to an existing project, or returns None if ConfigToml
    /// does not contain a project corresponding to cwd or the resolved git repo
    /// root for cwd.
    pub fn get_active_project(
        &self,
        resolved_cwd: &Path,
        repo_root: Option<&Path>,
    ) -> Option<ProjectConfig> {
        let mut lookup_keys = normalized_project_lookup_keys(resolved_cwd);
        if let Some(repo_root) = repo_root {
            lookup_keys.extend(normalized_project_lookup_keys(repo_root));
        }
        self.get_active_project_for_lookup_keys(&lookup_keys)
    }

    /// Resolves an active project from path keys already normalized by config loading.
    pub fn get_active_project_for_lookup_keys(
        &self,
        lookup_keys: &[String],
    ) -> Option<ProjectConfig> {
        let projects = ProjectLookup::new(self.projects.clone()?);
        lookup_keys
            .iter()
            .find_map(|lookup_key| projects.get(lookup_key).map(|(_, project)| project.clone()))
    }
}

/// Canonicalize the path and convert it to a string to be used as a key in the
/// projects trust map. On Windows, strips UNC, when possible, to try to ensure
/// that different paths that point to the same location have the same key.
pub(crate) fn normalized_project_lookup_keys(path: &Path) -> Vec<String> {
    let normalized_path = normalize_project_lookup_key(&path.to_string_lossy());
    let normalized_canonical_path = normalize_project_lookup_key(
        &normalize_for_path_comparison(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy(),
    );
    if normalized_path == normalized_canonical_path {
        vec![normalized_canonical_path]
    } else {
        vec![normalized_canonical_path, normalized_path]
    }
}

/// Preserves path case and drops a Windows verbatim prefix wherever the ordinary
/// spelling names the same location. The canonical key therefore stays in the `C:\...` form users
/// write, and `\\?\` keys persisted by earlier builds still match.
pub(crate) fn normalize_project_lookup_key(key: &str) -> String {
    normalize_for_native_workdir(key)
        .to_string_lossy()
        .into_owned()
}

pub(crate) struct ProjectLookup<T> {
    exact: HashMap<String, T>,
    normalized: OnceLock<BTreeMap<String, String>>,
}

impl<T> ProjectLookup<T> {
    pub(crate) fn new(exact: HashMap<String, T>) -> Self {
        Self {
            exact,
            normalized: OnceLock::new(),
        }
    }

    pub(crate) fn get(&self, lookup_key: &str) -> Option<(&str, &T)> {
        if let Some((key, value)) = self.exact.get_key_value(lookup_key) {
            return Some((key.as_str(), value));
        }
        // Exact spelling has priority and needs no filesystem equivalence work.
        // Build the deterministic fallback only when a lookup actually needs it.
        let normalized = self.normalized.get_or_init(|| {
            let mut keys = self.exact.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut normalized = BTreeMap::new();
            for key in keys {
                // Do not case-fold: Windows supports case-sensitive directories.
                if let Ok(canonical) = normalize_for_path_comparison(Path::new(&key)) {
                    normalized
                        .entry(normalize_project_lookup_key(&canonical.to_string_lossy()))
                        .or_insert_with(|| key.clone());
                }
                normalized
                    .entry(normalize_project_lookup_key(&key))
                    .or_insert(key);
            }
            normalized
        });
        let key = normalized.get(lookup_key)?;
        self.exact
            .get_key_value(key)
            .map(|(key, value)| (key.as_str(), value))
    }
}

pub fn validate_reserved_model_provider_ids(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    let mut conflicts = model_providers
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
            ) && RESERVED_MODEL_PROVIDER_IDS.contains(&key.as_str())
        })
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>();
    conflicts.sort_unstable();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "model_providers contains reserved built-in provider IDs: {}. \
Built-in providers cannot be overridden. Rename your custom provider (for example, `openai-custom`).",
            conflicts.join(", ")
        ))
    }
}

pub fn validate_model_providers(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    validate_reserved_model_provider_ids(model_providers)?;
    for (key, provider) in model_providers {
        if matches!(
            key.as_str(),
            AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
        ) {
            continue;
        }
        if provider.aws.is_some() {
            return Err(format!(
                "model_providers.{key}: provider aws is only supported for `{AMAZON_BEDROCK_PROVIDER_ID}` and `{AMAZON_BEDROCK_RUNTIME_PROVIDER_ID}`"
            ));
        }
        if provider.name.trim().is_empty() {
            return Err(format!(
                "model_providers.{key}: provider name must not be empty"
            ));
        }
        provider
            .validate()
            .map_err(|message| format!("model_providers.{key}: {message}"))?;
    }
    Ok(())
}

fn deserialize_model_providers<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, ModelProviderInfo>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model_providers = HashMap::<String, ModelProviderInfo>::deserialize(deserializer)?;
    validate_model_providers(&model_providers).map_err(serde::de::Error::custom)?;
    Ok(model_providers)
}
