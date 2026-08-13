use std::collections::BTreeSet;
use std::collections::HashSet;

use codex_analytics::SkillInvocation;
use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use tracing::instrument;

use crate::config::Config;
use crate::connectors;
use crate::tools::exposure::DirectMcpToolEntrypoint;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DirectMcpToolSelection {
    pub(crate) legacy_server_names: HashSet<String>,
    exact_tools: BTreeSet<DirectMcpToolEntrypoint>,
}

impl DirectMcpToolSelection {
    fn includes(&self, tool: &McpToolInfo) -> bool {
        self.legacy_server_names.contains(&tool.server_name)
            || self.exact_tools.contains(&DirectMcpToolEntrypoint {
                server_name: tool.server_name.clone(),
                tool_name: tool.tool.name.to_string(),
            })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SelectedSkillMcpExposure {
    pub(crate) selection: DirectMcpToolSelection,
    pub(crate) direct_entrypoints: Vec<DirectMcpToolEntrypoint>,
    pub(crate) diagnostics: Vec<String>,
}

pub(crate) fn resolve_selected_skill_mcp_exposure(
    selected_skills: &[SkillInvocation],
    loaded_plugins: &codex_core_plugins::PluginLoadOutcome,
    all_mcp_tools: &[McpToolInfo],
) -> SelectedSkillMcpExposure {
    let mut result = SelectedSkillMcpExposure::default();
    let selected_plugin_skills = selected_skills
        .iter()
        .filter_map(|invocation| {
            invocation
                .plugin_id
                .as_deref()
                .map(|plugin_id| (plugin_id, invocation.skill_name.as_str()))
        })
        .collect::<BTreeSet<_>>();
    for (plugin_id, skill_name) in selected_plugin_skills {
        let Some(plugin) = loaded_plugins
            .plugins()
            .iter()
            .find(|plugin| plugin.is_active() && plugin.config_name == plugin_id)
        else {
            continue;
        };

        let declaration = match plugin.tool_exposure.as_ref() {
            None => {
                result
                    .selection
                    .legacy_server_names
                    .extend(plugin.mcp_servers.keys().cloned());
                continue;
            }
            Some(codex_plugin::manifest::PluginToolExposure::Invalid(message)) => {
                result.diagnostics.push(format!(
                    "plugin `{plugin_id}` has invalid explicit toolExposure metadata: {message}; promoting nothing"
                ));
                continue;
            }
            Some(codex_plugin::manifest::PluginToolExposure::Valid(config)) => {
                let Some(declaration) = config.skills.get(skill_name) else {
                    result
                        .selection
                        .legacy_server_names
                        .extend(plugin.mcp_servers.keys().cloned());
                    continue;
                };
                declaration
            }
        };

        let declared_tools = declaration
            .mcp_tools
            .iter()
            .flat_map(|(server_name, tool_names)| {
                tool_names
                    .iter()
                    .map(move |tool_name| DirectMcpToolEntrypoint {
                        server_name: server_name.clone(),
                        tool_name: tool_name.clone(),
                    })
            })
            .collect::<BTreeSet<_>>();
        let unavailable = declared_tools
            .iter()
            .filter(|entrypoint| {
                !plugin.mcp_servers.contains_key(&entrypoint.server_name)
                    || !all_mcp_tools.iter().any(|tool| {
                        tool.server_name == entrypoint.server_name
                            && tool.tool.name.as_ref() == entrypoint.tool_name
                            && tool_is_model_visible(tool)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !unavailable.is_empty() {
            for entrypoint in unavailable {
                result.diagnostics.push(format!(
                    "plugin `{plugin_id}` skill `{skill_name}` declares unavailable MCP entrypoint `{}/{}`",
                    entrypoint.server_name, entrypoint.tool_name
                ));
            }
            result.diagnostics.push(format!(
                "ignoring the invalid explicit toolExposure declaration for plugin `{plugin_id}` skill `{skill_name}`"
            ));
            continue;
        }
        result.selection.exact_tools.extend(declared_tools);
    }

    let visible_tools = all_mcp_tools
        .iter()
        .filter(|tool| result.selection.includes(tool))
        .map(|tool| DirectMcpToolEntrypoint {
            server_name: tool.server_name.clone(),
            tool_name: tool.tool.name.to_string(),
        })
        .collect::<BTreeSet<_>>();
    result.direct_entrypoints = visible_tools.into_iter().collect();
    result
}

pub(crate) struct McpToolExposure {
    pub(crate) direct_tools: Vec<McpToolInfo>,
    pub(crate) deferred_tools: Option<Vec<McpToolInfo>>,
}

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_mcp_tool_exposure(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    config: &Config,
    search_tool_enabled: bool,
    direct_mcp_tools: &DirectMcpToolSelection,
) -> McpToolExposure {
    let mut deferred_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    if let Some(connectors) = connectors {
        deferred_tools.extend(filter_codex_apps_mcp_tools(
            all_mcp_tools,
            connectors,
            config,
        ));
    }

    if !search_tool_enabled {
        return McpToolExposure {
            direct_tools: deferred_tools,
            deferred_tools: None,
        };
    }

    let (direct_tools, deferred_tools) = deferred_tools
        .into_iter()
        .partition(|tool| direct_mcp_tools.includes(tool));

    McpToolExposure {
        direct_tools,
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
    }
}

fn filter_non_codex_apps_mcp_tools_only(mcp_tools: &[McpToolInfo]) -> Vec<McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
        })
        .cloned()
        .collect()
}

fn filter_codex_apps_mcp_tools(
    mcp_tools: &[McpToolInfo],
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> Vec<McpToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools
        .iter()
        .filter(|tool| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return false;
            }
            if !tool_is_model_visible(tool) {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            let annotations = tool.tool.annotations.as_ref();
            allowed.contains(connector_id)
                && app_tool_policy
                    .policy(AppToolPolicyInput {
                        connector_id: Some(connector_id),
                        tool_name: &tool.tool.name,
                        tool_title: tool.tool.title.as_deref(),
                        destructive_hint: annotations
                            .and_then(|annotations| annotations.destructive_hint),
                        open_world_hint: annotations
                            .and_then(|annotations| annotations.open_world_hint),
                    })
                    .enabled
        })
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
