//! Centralized feature flags and metadata.
//!
//! This crate defines the feature registry plus the logic used to resolve an
//! effective feature set from config-like inputs.

use codex_otel::SessionTelemetry;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use toml::Table;

/// Minimum supported per-session thread cap for multi-agent v2.
pub const MULTI_AGENT_V2_MIN_CONCURRENT_THREADS_PER_SESSION: usize = 1;
/// Default per-session thread cap for multi-agent v2, including the root agent.
pub const MULTI_AGENT_V2_DEFAULT_MAX_CONCURRENT_THREADS_PER_SESSION: usize = 3;
/// Minimum supported wait timeout for multi-agent tools.
pub const MULTI_AGENT_MIN_WAIT_TIMEOUT_MS: i64 = 60_000;
/// Maximum supported wait timeout for multi-agent tools.
pub const MULTI_AGENT_MAX_WAIT_TIMEOUT_MS: i64 = 60 * 60 * 1000;
/// Default wait timeout for multi-agent tools.
pub const MULTI_AGENT_DEFAULT_WAIT_TIMEOUT_MS: i64 = MULTI_AGENT_MIN_WAIT_TIMEOUT_MS;

mod feature_configs;
pub use feature_configs::CodeModeConfigToml;
pub use feature_configs::CurrentTimeReminderConfigToml;
pub use feature_configs::CurrentTimeReminderDeliveryMode;
pub use feature_configs::CurrentTimeSource;
pub use feature_configs::MultiAgentV2ConfigToml;
pub use feature_configs::NetworkProxyConfigToml;
pub use feature_configs::NetworkProxyDomainPermissionToml;
pub use feature_configs::NetworkProxyModeToml;
pub use feature_configs::NetworkProxyUnixSocketPermissionToml;
use feature_configs::RemovedAppsMcpPathOverrideConfigToml;

/// High-level lifecycle stage for a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Features that are still under development, not ready for external use
    UnderDevelopment,
    /// Experimental features made available to users through the `/experimental` menu
    Experimental {
        name: &'static str,
        menu_description: &'static str,
        announcement: &'static str,
    },
    /// Stable features. The feature flag is kept for ad-hoc enabling/disabling
    Stable,
    /// Deprecated feature that should not be used anymore.
    Deprecated,
    /// Internal runtime state that is not accepted from user configuration.
    Internal,
}

impl Stage {
    pub fn experimental_menu_name(self) -> Option<&'static str> {
        match self {
            Stage::Experimental { name, .. } => Some(name),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Internal => None,
        }
    }

    pub fn experimental_menu_description(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                menu_description, ..
            } => Some(menu_description),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Internal => None,
        }
    }

    pub fn experimental_announcement(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                announcement: "", ..
            } => None,
            Stage::Experimental { announcement, .. } => Some(announcement),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Internal => None,
        }
    }
}

/// Unique features toggled via configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    // Stable.
    /// Enable the default shell tool.
    ShellTool,
    /// Enable Claude-style lifecycle hooks loaded from hooks.json files.
    CodexHooks,
    /// Store CLI auth in the encrypted local secrets backend when keyring storage is selected.
    SecretAuthStorage,

    // Experimental
    /// Enable JavaScript code mode backed by the in-process V8 runtime.
    CodeMode,
    /// Run JavaScript code mode in the standalone host process.
    CodeModeHost,
    /// Restrict model-visible tools to code mode entrypoints (`exec`, `wait`).
    CodeModeOnly,
    /// Use the single unified PTY-backed exec tool.
    UnifiedExec,
    /// Add terminal-specific visualization guidance to TUI developer instructions.
    TerminalVisualizationInstructions,
    /// Stream structured progress while apply_patch input is being generated.
    ApplyPatchStreamingEvents,
    /// Allow exec tools to request additional permissions while staying sandboxed.
    ExecPermissionApprovals,
    /// Expose the built-in request_permissions tool.
    RequestPermissionsTool,
    /// Allow the model to request web searches that fetch live content.
    WebSearchRequest,
    /// Allow the model to request web searches that fetch cached content.
    /// Takes precedence over `WebSearchRequest`.
    WebSearchCached,
    /// Expose the extension-backed standalone web search tool.
    StandaloneWebSearch,
    /// Experimental shell snapshotting.
    ShellSnapshot,
    /// Collect and, after shadow validation, reuse immutable tool output evidence.
    KnownDeltaStore,
    /// Allow turns to start while selected executors are still starting.
    DeferredExecutor,
    /// Enable runtime metrics snapshots via a manual reader.
    RuntimeMetrics,
    /// Enable startup memory extraction and file-backed memory consolidation.
    MemoryTool,
    /// Compress cold local thread-store rollout files.
    LocalThreadStoreCompression,
    /// Compress request bodies (zstd) when sending streaming requests to codex-backend.
    EnableRequestCompression,
    /// Start the managed network proxy for sandboxed sessions.
    NetworkProxy,
    /// Respect host system proxy settings for Codex-owned network clients.
    RespectSystemProxy,
    /// Enable collab tools.
    Collab,
    /// Enable task-path-based multi-agent routing.
    MultiAgentV2,
    /// Enable CSV-backed agent job tools.
    SpawnCsv,
    /// Enable apps.
    Apps,
    /// Expose MCP model-visible namespaces without the legacy `mcp__` prefix.
    NonPrefixedMcpToolNames,
    /// Enable discoverable tool suggestions for apps.
    ToolSuggest,
    /// Enable plugins.
    Plugins,
    /// Allow the in-app browser pane in desktop apps.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    InAppBrowser,
    /// Allow Browser Use agent integration in desktop apps.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUse,
    /// Allow Browser Use integration to access the full Chrome DevTools Protocol surface.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUseFullCdpAccess,
    /// Allow Browser Use integration with external browsers.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    BrowserUseExternal,
    /// Allow Codex Computer Use.
    ///
    /// Requirements-only gate: this should be set from requirements, not user config.
    ComputerUse,
    /// Enable the PS-backed remote plugin catalog.
    RemotePlugin,
    /// Enable remote plugin sharing flows.
    PluginSharing,
    /// Enable extension-backed image generation.
    ImageGeneration,
    /// Request sequential cutoff reasoning summary delivery.
    ConcurrentReasoningSummaries,
    /// Allow prompting and installing missing MCP dependencies.
    SkillMcpDependencyInstall,
    /// Allow request_user_input in Default collaboration mode.
    DefaultModeRequestUserInput,
    /// Enable automatic review for approval prompts.
    GuardianApproval,
    /// Run an independent KD4 completion review before eligible root turns finish.
    TaskCompletionReviewer,
    /// Enable persisted thread goals and automatic goal continuation.
    Goals,
    /// Add current-time reminders to model-visible context.
    CurrentTimeReminder,
    /// Route MCP tool approval prompts through the MCP elicitation request path.
    ToolCallMcpElicitation,
    /// Prompt Codex Apps connector auth failures through MCP URL elicitations.
    AuthElicitation,
    /// Enable personality selection in the TUI.
    Personality,
    /// Enable Fast mode selection in the TUI and request layer.
    FastMode,
    /// Prevent idle system sleep while a turn is actively running.
    PreventIdleSleep,
    /// Use Agent Identity for ChatGPT-authenticated sessions.
    UseAgentIdentity,

    // Internal-only features retained after removal from user configuration.
    /// Enable Windows sandbox (restricted token) on Windows.
    WindowsSandbox,
    /// Use the elevated Windows sandbox pipeline (setup + runner).
    WindowsSandboxElevated,
}

impl Feature {
    pub fn key(self) -> &'static str {
        self.info().key
    }

    pub fn stage(self) -> Stage {
        self.info().stage
    }

    pub fn default_enabled(self) -> bool {
        self.info().default_enabled
    }

    fn info(self) -> FeatureSpec {
        feature_info(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegacyFeatureUsage {
    pub alias: String,
    pub feature: Feature,
    pub summary: String,
    pub details: Option<String>,
}

/// Holds the effective set of enabled features.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    enabled: BTreeSet<Feature>,
    legacy_usages: BTreeSet<LegacyFeatureUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct FeatureOverrides {
    pub web_search_request: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureConfigSource<'a> {
    pub features: Option<&'a FeaturesToml>,
}

impl FeatureOverrides {
    fn apply(self, features: &mut Features) {
        if let Some(enabled) = self.web_search_request {
            if enabled {
                features.enable(Feature::WebSearchRequest);
            } else {
                features.disable(Feature::WebSearchRequest);
            }
            features.record_legacy_usage("web_search_request", Feature::WebSearchRequest);
        }
    }
}

impl Features {
    /// Starts with built-in defaults.
    pub fn with_defaults() -> Self {
        let mut set = BTreeSet::new();
        for spec in FEATURES {
            if spec.default_enabled {
                set.insert(spec.id);
            }
        }
        Self {
            enabled: set,
            legacy_usages: BTreeSet::new(),
        }
    }

    pub fn enabled(&self, f: Feature) -> bool {
        self.enabled.contains(&f)
    }

    pub fn apps_enabled_for_auth(&self, has_chatgpt_auth: bool) -> bool {
        self.enabled(Feature::Apps) && has_chatgpt_auth
    }

    pub fn enable(&mut self, f: Feature) -> &mut Self {
        self.enabled.insert(f);
        self
    }

    pub fn disable(&mut self, f: Feature) -> &mut Self {
        self.enabled.remove(&f);
        self
    }

    pub fn set_enabled(&mut self, f: Feature, enabled: bool) -> &mut Self {
        if enabled {
            self.enable(f)
        } else {
            self.disable(f)
        }
    }

    pub fn record_legacy_usage_force(&mut self, alias: &str, feature: Feature) {
        let (summary, details) = legacy_usage_notice(alias, feature);
        self.legacy_usages.insert(LegacyFeatureUsage {
            alias: alias.to_string(),
            feature,
            summary,
            details,
        });
    }

    pub fn record_legacy_usage(&mut self, alias: &str, feature: Feature) {
        if alias == feature.key() {
            return;
        }
        self.record_legacy_usage_force(alias, feature);
    }

    pub fn legacy_feature_usages(&self) -> impl Iterator<Item = &LegacyFeatureUsage> + '_ {
        self.legacy_usages.iter()
    }

    pub fn emit_metrics(&self, otel: &SessionTelemetry) {
        for feature in FEATURES {
            if matches!(feature.stage, Stage::Internal) {
                continue;
            }
            if self.enabled(feature.id) != feature.default_enabled {
                otel.counter(
                    "codex.feature.state",
                    /*inc*/ 1,
                    &[
                        ("feature", feature.key),
                        ("value", &self.enabled(feature.id).to_string()),
                    ],
                );
            }
        }
    }

    /// Apply a table of key -> bool toggles (e.g. from TOML).
    pub fn apply_map(&mut self, m: &BTreeMap<String, bool>) {
        for (k, v) in m {
            match k.as_str() {
                "web_search_request" => {
                    self.record_legacy_usage_force(
                        "features.web_search_request",
                        Feature::WebSearchRequest,
                    );
                }
                "web_search_cached" => {
                    self.record_legacy_usage_force(
                        "features.web_search_cached",
                        Feature::WebSearchCached,
                    );
                }
                _ => {}
            }
            let feature = canonical_feature_for_key(k)
                .filter(|feature| !matches!(feature.stage(), Stage::Internal));
            match feature {
                Some(feat) => {
                    if *v {
                        self.enable(feat);
                    } else {
                        self.disable(feat);
                    }
                }
                None => {
                    tracing::warn!("unknown feature key in config: {k}");
                }
            }
        }
    }

    pub fn from_sources(
        base: FeatureConfigSource<'_>,
        profile: FeatureConfigSource<'_>,
        overrides: FeatureOverrides,
    ) -> Self {
        let mut features = Features::with_defaults();

        for source in [base, profile] {
            if let Some(feature_entries) = source.features {
                features.apply_toml(feature_entries);
            }
        }

        overrides.apply(&mut features);
        features.normalize_dependencies();

        features
    }

    pub fn enabled_features(&self) -> Vec<Feature> {
        self.enabled.iter().copied().collect()
    }

    pub fn normalize_dependencies(&mut self) {
        if self.enabled(Feature::SpawnCsv) && !self.enabled(Feature::Collab) {
            self.enable(Feature::Collab);
        }
        if self.enabled(Feature::CodeModeOnly) && !self.enabled(Feature::CodeMode) {
            self.enable(Feature::CodeMode);
        }
    }
}

fn legacy_usage_notice(alias: &str, feature: Feature) -> (String, Option<String>) {
    let canonical = feature.key();
    match feature {
        Feature::WebSearchRequest | Feature::WebSearchCached => {
            let label = match alias {
                "web_search" => "[features].web_search",
                "features.web_search_request" | "web_search_request" => {
                    "[features].web_search_request"
                }
                "features.web_search_cached" | "web_search_cached" => {
                    "[features].web_search_cached"
                }
                _ => alias,
            };
            let summary =
                format!("`{label}` is deprecated because web search is enabled by default.");
            (summary, Some(web_search_details().to_string()))
        }
        _ => {
            let label = if alias.contains('.') || alias.starts_with('[') {
                alias.to_string()
            } else {
                format!("[features].{alias}")
            };
            let summary = format!("`{label}` is deprecated. Use `[features].{canonical}` instead.");
            let details = if alias == canonical {
                None
            } else {
                Some(format!(
                    "Enable it with `--enable {canonical}` or `[features].{canonical}` in config.toml. See https://developers.openai.com/codex/config-basic#feature-flags for details."
                ))
            };
            (summary, details)
        }
    }
}

fn web_search_details() -> &'static str {
    "Set `web_search` to `\"live\"`, `\"indexed\"`, `\"cached\"`, or `\"disabled\"` at the top level (or under a profile) in config.toml if you want to override it."
}

/// Returns the feature registered for a canonical key, including internal-only keys.
///
/// Use [`user_settable_feature_for_key`] when validating user configuration.
pub fn feature_for_key(key: &str) -> Option<Feature> {
    canonical_feature_for_key(key)
}

pub fn canonical_feature_for_key(key: &str) -> Option<Feature> {
    FEATURES
        .iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.id)
}

/// Canonical feature keys that may be changed by current user-facing APIs.
///
/// Internal-only features are not advertised or written into new configuration.
pub fn user_settable_feature_for_key(key: &str) -> Option<Feature> {
    canonical_feature_for_key(key).filter(|feature| !matches!(feature.stage(), Stage::Internal))
}

/// Feature definitions exposed by current catalogs and listing commands.
pub fn user_settable_features() -> impl Iterator<Item = &'static FeatureSpec> {
    FEATURES
        .iter()
        .filter(|spec| !matches!(spec.stage, Stage::Internal))
}

/// Returns `true` if the provided string matches a known feature toggle key.
pub fn is_known_feature_key(key: &str) -> bool {
    user_settable_feature_for_key(key).is_some()
}

/// Deserializable features table for TOML.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct FeaturesToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_mode: Option<FeatureToml<CodeModeConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_v2: Option<FeatureToml<MultiAgentV2ConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_time_reminder: Option<FeatureToml<CurrentTimeReminderConfigToml>>,
    #[serde(default, rename = "apps_mcp_path_override", skip_serializing)]
    #[schemars(skip)]
    removed_apps_mcp_path_override: Option<FeatureToml<RemovedAppsMcpPathOverrideConfigToml>>,
    pub network_proxy: Option<FeatureToml<NetworkProxyConfigToml>>,
    /// Boolean feature toggles keyed by canonical feature name.
    #[serde(flatten)]
    entries: BTreeMap<String, bool>,
}

impl Features {
    fn apply_toml(&mut self, features: &FeaturesToml) {
        let entries = features.entries();
        self.apply_map(&entries);
    }
}

impl FeaturesToml {
    /// Removes compatibility-only inputs that no longer affect runtime
    /// behavior or belong in newly materialized config.
    pub fn clear_removed_compatibility_entries(&mut self) {
        self.removed_apps_mcp_path_override = None;
        self.entries.remove("apps_mcp_path_override");
    }

    pub fn entries(&self) -> BTreeMap<String, bool> {
        let mut entries = self.entries.clone();
        if let Some(enabled) = self.code_mode.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::CodeMode.key().to_string(), enabled);
        }
        if let Some(enabled) = self.multi_agent_v2.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::MultiAgentV2.key().to_string(), enabled);
        }
        if let Some(enabled) = self
            .current_time_reminder
            .as_ref()
            .and_then(FeatureToml::enabled)
        {
            entries.insert(Feature::CurrentTimeReminder.key().to_string(), enabled);
        }
        if let Some(enabled) = self.network_proxy.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::NetworkProxy.key().to_string(), enabled);
        }
        entries
    }

    pub fn materialize_resolved_enabled(&mut self, features: &Features) {
        self.clear_removed_compatibility_entries();
        let Self {
            code_mode,
            multi_agent_v2,
            current_time_reminder,
            removed_apps_mcp_path_override: _,
            network_proxy,
            entries,
        } = self;
        for spec in FEATURES {
            if matches!(spec.stage, Stage::Internal) {
                entries.remove(spec.key);
                continue;
            }
            let enabled = features.enabled(spec.id);
            if spec.id == Feature::CodeMode {
                materialize_resolved_feature_enabled(code_mode, enabled);
            } else if spec.id == Feature::MultiAgentV2 {
                materialize_resolved_feature_enabled(multi_agent_v2, enabled);
            } else if spec.id == Feature::CurrentTimeReminder {
                materialize_resolved_feature_enabled(current_time_reminder, enabled);
            } else if spec.id == Feature::NetworkProxy {
                materialize_resolved_feature_enabled(network_proxy, enabled);
            } else {
                entries.insert(spec.key.to_string(), enabled);
            }
        }
    }
}

fn materialize_resolved_feature_enabled<T: FeatureConfig>(
    feature: &mut Option<FeatureToml<T>>,
    enabled: bool,
) {
    match feature {
        Some(feature) => feature.set_enabled(enabled),
        None => *feature = Some(FeatureToml::Enabled(enabled)),
    }
}

impl From<BTreeMap<String, bool>> for FeaturesToml {
    fn from(entries: BTreeMap<String, bool>) -> Self {
        Self {
            entries,
            ..Default::default()
        }
    }
}

// To be used for features that need more configuration than just enabled/disabled and
// require a custom config struct under `[features]`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum FeatureToml<T> {
    Enabled(bool),
    Config(T),
}

impl<T: FeatureConfig> FeatureToml<T> {
    pub fn enabled(&self) -> Option<bool> {
        match self {
            Self::Enabled(enabled) => Some(*enabled),
            Self::Config(config) => config.enabled(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        match self {
            Self::Enabled(value) => *value = enabled,
            Self::Config(config) => config.set_enabled(enabled),
        }
    }
}

// A trait to be implemented by custom feature config structs when defining a feature that needs more configuration than
// just enabled/disabled.
pub trait FeatureConfig {
    fn enabled(&self) -> Option<bool>;
    fn set_enabled(&mut self, enabled: bool);
}

/// Single, easy-to-read registry of all feature definitions.
#[derive(Debug, Clone, Copy)]
pub struct FeatureSpec {
    pub id: Feature,
    pub key: &'static str,
    pub stage: Stage,
    pub default_enabled: bool,
}

/// Stable machine-readable projection of the authoritative feature registry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureRegistryEntry {
    pub key: &'static str,
    pub default_enabled: bool,
}

pub fn feature_registry_entries() -> Vec<FeatureRegistryEntry> {
    FEATURES
        .iter()
        .map(|spec| FeatureRegistryEntry {
            key: spec.key,
            default_enabled: spec.default_enabled,
        })
        .collect()
}

macro_rules! define_features {
    ($(
        FeatureSpec {
            id: Feature::$id:ident,
            key: $key:expr,
            stage: $stage:expr,
            default_enabled: $default_enabled:expr,
        }
    ),* $(,)?) => {
        pub const FEATURES: &[FeatureSpec] = &[
            $(FeatureSpec {
                id: Feature::$id,
                key: $key,
                stage: $stage,
                default_enabled: $default_enabled,
            }),*
        ];

        fn feature_info(feature: Feature) -> FeatureSpec {
            match feature {
                $(Feature::$id => FeatureSpec {
                    id: Feature::$id,
                    key: $key,
                    stage: $stage,
                    default_enabled: $default_enabled,
                }),*
            }
        }

        #[cfg(test)]
        const ALL_FEATURES: &[Feature] = &[$(Feature::$id),*];
    };
}

define_features! {
    // Stable features.
    FeatureSpec {
        id: Feature::ShellTool,
        key: "shell_tool",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::SecretAuthStorage,
        key: "secret_auth_storage",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::UnifiedExec,
        key: "unified_exec",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ShellSnapshot,
        key: "shell_snapshot",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::KnownDeltaStore,
        key: "known_delta_store",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::DeferredExecutor,
        key: "deferred_executor",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CodeMode,
        key: "code_mode",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CodeModeHost,
        key: "code_mode_host",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CodeModeOnly,
        key: "code_mode_only",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::WebSearchRequest,
        key: "web_search_request",
        stage: Stage::Deprecated,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::WebSearchCached,
        key: "web_search_cached",
        stage: Stage::Deprecated,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::StandaloneWebSearch,
        key: "standalone_web_search",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RuntimeMetrics,
        key: "runtime_metrics",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::MemoryTool,
        key: "memories",
        stage: Stage::Stable,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::LocalThreadStoreCompression,
        key: "local_thread_store_compression",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ApplyPatchStreamingEvents,
        key: "apply_patch_streaming_events",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ExecPermissionApprovals,
        key: "exec_permission_approvals",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CodexHooks,
        key: "hooks",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::RequestPermissionsTool,
        key: "request_permissions_tool",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::WindowsSandbox,
        key: "experimental_windows_sandbox",
        stage: Stage::Internal,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::WindowsSandboxElevated,
        key: "elevated_windows_sandbox",
        stage: Stage::Internal,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::EnableRequestCompression,
        key: "enable_request_compression",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::NetworkProxy,
        key: "network_proxy",
        stage: Stage::Experimental {
            name: "Network proxy",
            menu_description: "Apply network proxy restrictions to sandboxed sessions that already have network access.",
            announcement: "NEW: Network proxy can now be enabled from /experimental. Restart Codex after enabling it.",
        },
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RespectSystemProxy,
        key: "respect_system_proxy",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Collab,
        key: "multi_agent",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::MultiAgentV2,
        key: "multi_agent_v2",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::SpawnCsv,
        key: "enable_fanout",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Apps,
        key: "apps",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::NonPrefixedMcpToolNames,
        key: "non_prefixed_mcp_tool_names",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ToolSuggest,
        key: "tool_suggest",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::Plugins,
        key: "plugins",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::InAppBrowser,
        key: "in_app_browser",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUse,
        key: "browser_use",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUseFullCdpAccess,
        key: "browser_use_full_cdp_access",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::BrowserUseExternal,
        key: "browser_use_external",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ComputerUse,
        key: "computer_use",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::RemotePlugin,
        key: "remote_plugin",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::PluginSharing,
        key: "plugin_sharing",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ImageGeneration,
        key: "image_generation",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ConcurrentReasoningSummaries,
        key: "concurrent_reasoning_summaries",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SkillMcpDependencyInstall,
        key: "skill_mcp_dependency_install",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::DefaultModeRequestUserInput,
        key: "default_mode_request_user_input",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::TerminalVisualizationInstructions,
        key: "terminal_visualization_instructions",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::GuardianApproval,
        key: "guardian_approval",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::TaskCompletionReviewer,
        key: "task_completion_reviewer",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::Goals,
        key: "goals",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::CurrentTimeReminder,
        key: "current_time_reminder",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ToolCallMcpElicitation,
        key: "tool_call_mcp_elicitation",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::AuthElicitation,
        key: "auth_elicitation",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::Personality,
        key: "personality",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::FastMode,
        key: "fast_mode",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::PreventIdleSleep,
        key: "prevent_idle_sleep",
        stage: Stage::Experimental {
            name: "Prevent sleep while running",
            menu_description: "Keep your computer awake while Codex is running a thread.",
            announcement: "NEW: Prevent sleep while running is now available in /experimental.",
        },
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::UseAgentIdentity,
        key: "use_agent_identity",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
}

pub fn unstable_features_warning_event(
    effective_features: Option<&Table>,
    suppress_unstable_features_warning: bool,
    features: &Features,
    config_path: &str,
) -> Option<Event> {
    if suppress_unstable_features_warning {
        return None;
    }

    let mut under_development_feature_keys = BTreeSet::new();
    if let Some(table) = effective_features {
        for (key, value) in table {
            let is_enabled = value.as_bool() == Some(true)
                || value
                    .as_table()
                    .and_then(|table| table.get("enabled"))
                    .and_then(toml::Value::as_bool)
                    == Some(true);
            if !is_enabled {
                continue;
            }
            let Some(feature) = feature_for_key(key) else {
                continue;
            };
            if !features.enabled(feature) {
                continue;
            }
            if matches!(feature.stage(), Stage::UnderDevelopment) {
                under_development_feature_keys.insert(feature.key());
            }
        }
    }

    if under_development_feature_keys.is_empty() {
        return None;
    }

    let under_development_feature_keys = under_development_feature_keys
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "Under-development features enabled: {under_development_feature_keys}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {config_path}."
    );
    Some(Event {
        id: String::new(),
        msg: EventMsg::Warning(WarningEvent { message }),
    })
}

#[cfg(test)]
mod tests;
