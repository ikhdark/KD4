use crate::FeatureConfig;
use schemars::JsonSchema;
use schemars::r#gen::SchemaGenerator;
use schemars::schema::InstanceType;
use schemars::schema::NumberValidation;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovedAppsMcpPathOverrideConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeModeConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Exact tool namespaces to omit from the code-mode nested tool surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_tool_namespaces: Option<Vec<String>>,
    /// Exact tool namespaces to expose only as direct model tools.
    /// These tools bypass deferral, remain top-level in code-mode-only sessions, and are omitted
    /// from the nested code-mode tool surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_only_tool_namespaces: Option<Vec<String>>,
}

impl FeatureConfig for CodeModeConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = Some(enabled);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MultiAgentV2ConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "multi_agent_v2_concurrency_schema")]
    pub max_concurrent_threads_per_session: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "multi_agent_wait_timeout_schema")]
    pub min_wait_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "multi_agent_wait_timeout_schema")]
    pub max_wait_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "multi_agent_wait_timeout_schema")]
    pub default_wait_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_agent_usage_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_usage_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_agent_mode_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-zA-Z0-9_-]+$"))]
    pub tool_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_spawn_agent_metadata: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_code_mode_only: Option<bool>,
    /// Allows the high-token-cost `fork_turns = "all"` mode. Disabled by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_full_history_forks: Option<bool>,
}

fn multi_agent_v2_concurrency_schema(_generator: &mut SchemaGenerator) -> Schema {
    integer_schema(
        crate::MULTI_AGENT_V2_MIN_CONCURRENT_THREADS_PER_SESSION as i64,
        None,
    )
}

fn multi_agent_wait_timeout_schema(_generator: &mut SchemaGenerator) -> Schema {
    integer_schema(
        crate::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS,
        Some(crate::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS),
    )
}

fn integer_schema(minimum: i64, maximum: Option<i64>) -> Schema {
    Schema::Object(SchemaObject {
        instance_type: Some(InstanceType::Integer.into()),
        number: Some(Box::new(NumberValidation {
            minimum: Some(minimum as f64),
            maximum: maximum.map(|value| value as f64),
            ..Default::default()
        })),
        ..Default::default()
    })
}

impl FeatureConfig for MultiAgentV2ConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = Some(enabled);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrentTimeSource {
    #[default]
    System,
    External,
}

/// Which inference boundaries may receive current-time reminders.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrentTimeReminderDeliveryMode {
    /// Allow a reminder before any inference request once the interval is due.
    #[default]
    AnyInference,
    /// Allow reminders after user input or tool output; new context windows still force one.
    AfterUserOrToolOutput,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CurrentTimeReminderConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reminder_interval_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_source: Option<CurrentTimeSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<CurrentTimeReminderDeliveryMode>,
    /// Expose the input-interruptible `clock.sleep` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleep_tool: Option<bool>,
}

impl FeatureConfig for CurrentTimeReminderConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = Some(enabled);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkProxyConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_socks5: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_socks5_udp: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_upstream_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dangerously_allow_non_loopback_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dangerously_allow_all_unix_sockets: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkProxyModeToml>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domains: Option<BTreeMap<String, NetworkProxyDomainPermissionToml>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix_sockets: Option<BTreeMap<String, NetworkProxyUnixSocketPermissionToml>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
}

impl FeatureConfig for NetworkProxyConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = Some(enabled);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProxyModeToml {
    Limited,
    Full,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProxyDomainPermissionToml {
    Allow,
    Deny,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProxyUnixSocketPermissionToml {
    Allow,
    Deny,
}
