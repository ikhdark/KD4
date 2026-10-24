use std::path::PathBuf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum ExternalAgentConfigMigrationItemType {
    #[serde(rename = "AGENTS_MD")]
    #[ts(rename = "AGENTS_MD")]
    AgentsMd,
    #[serde(rename = "CONFIG")]
    #[ts(rename = "CONFIG")]
    Config,
    #[serde(rename = "SKILLS")]
    #[ts(rename = "SKILLS")]
    Skills,
    #[serde(rename = "PLUGINS")]
    #[ts(rename = "PLUGINS")]
    Plugins,
    #[serde(rename = "MCP_SERVER_CONFIG")]
    #[ts(rename = "MCP_SERVER_CONFIG")]
    McpServerConfig,
    #[serde(rename = "SUBAGENTS")]
    #[ts(rename = "SUBAGENTS")]
    Subagents,
    #[serde(rename = "HOOKS")]
    #[ts(rename = "HOOKS")]
    Hooks,
    #[serde(rename = "COMMANDS")]
    #[ts(rename = "COMMANDS")]
    Commands,
    #[serde(rename = "SESSIONS")]
    #[ts(rename = "SESSIONS")]
    Sessions,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct PluginsMigration {
    #[serde(rename = "marketplaceName")]
    #[ts(rename = "marketplaceName")]
    pub marketplace_name: String,
    #[serde(rename = "pluginNames")]
    #[ts(rename = "pluginNames")]
    pub plugin_names: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillMigration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionMigration {
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct McpServerMigration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct HookMigration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SubagentMigration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CommandMigration {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MigrationDetails {
    #[serde(default)]
    pub plugins: Vec<PluginsMigration>,
    #[serde(default)]
    pub skills: Vec<SkillMigration>,
    #[serde(default)]
    pub sessions: Vec<SessionMigration>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerMigration>,
    #[serde(default)]
    pub hooks: Vec<HookMigration>,
    #[serde(default)]
    pub subagents: Vec<SubagentMigration>,
    #[serde(default)]
    pub commands: Vec<CommandMigration>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigMigrationItem {
    pub item_type: ExternalAgentConfigMigrationItemType,
    pub description: String,
    /// Null or empty means home-scoped migration; non-empty means repo-scoped migration.
    pub cwd: Option<PathBuf>,
    pub details: Option<MigrationDetails>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigDetectResponse {
    pub items: Vec<ExternalAgentConfigMigrationItem>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigDetectParams {
    /// If true, include detection under the user's home directory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_home: bool,
    /// Zero or more working directories to include for repo-scoped detection.
    #[ts(optional = nullable)]
    pub cwds: Option<Vec<PathBuf>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportParams {
    pub migration_items: Vec<ExternalAgentConfigMigrationItem>,
    /// Source product that produced the migration items. Missing means unspecified.
    #[ts(optional = nullable)]
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportResponse {
    pub import_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportItemTypeFailure {
    pub item_type: ExternalAgentConfigMigrationItemType,
    pub error_type: Option<String>,
    pub failure_stage: String,
    pub message: String,
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportItemTypeSuccess {
    pub item_type: ExternalAgentConfigMigrationItemType,
    pub cwd: Option<PathBuf>,
    pub source: Option<String>,
    pub target: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportTypeResult {
    pub item_type: ExternalAgentConfigMigrationItemType,
    pub successes: Vec<ExternalAgentConfigImportItemTypeSuccess>,
    pub failures: Vec<ExternalAgentConfigImportItemTypeFailure>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportHistory {
    pub import_id: String,
    pub completed_at_ms: i64,
    pub successes: Vec<ExternalAgentConfigImportItemTypeSuccess>,
    pub failures: Vec<ExternalAgentConfigImportItemTypeFailure>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportHistoriesReadResponse {
    pub data: Vec<ExternalAgentConfigImportHistory>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportProgressNotification {
    pub import_id: String,
    pub item_type_results: Vec<ExternalAgentConfigImportTypeResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalAgentConfigImportCompletedNotification {
    pub import_id: String,
    pub item_type_results: Vec<ExternalAgentConfigImportTypeResult>,
}

