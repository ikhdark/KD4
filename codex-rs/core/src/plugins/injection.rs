use std::collections::BTreeSet;

use codex_connectors::ConnectorSnapshot;
use codex_connectors::metadata::connector_display_label;
use codex_protocol::models::ResponseItem;

use crate::connectors;
use crate::context::ContextualUserFragment;
use crate::context::PluginInstructions;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::render_explicit_plugin_instructions;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpServerSource;
use codex_mcp::ResolvedMcpCatalog;
use codex_mcp::ToolInfo;

pub(crate) fn build_plugin_injections(
    mentioned_plugins: &[PluginCapabilitySummary],
    mcp_tools: &[ToolInfo],
    available_connectors: &[connectors::AppInfo],
    mcp_server_catalog: &ResolvedMcpCatalog,
    connector_snapshot: &ConnectorSnapshot,
) -> Vec<ResponseItem> {
    if mentioned_plugins.is_empty() {
        return Vec::new();
    }

    // Turn each explicit plugin mention into a developer hint that points the
    // model at the plugin's visible MCP servers, enabled apps, and skill prefix.
    mentioned_plugins
        .iter()
        .filter_map(|plugin| {
            let available_mcp_servers = mcp_tools
                .iter()
                .filter(|tool| {
                    tool.server_name != CODEX_APPS_MCP_SERVER_NAME
                        && mcp_server_catalog
                            .server(tool.server_name.as_str())
                            .is_some_and(|server| match server.source() {
                                McpServerSource::Plugin(attribution)
                                | McpServerSource::SelectedPlugin(attribution) => {
                                    attribution.plugin_id() == plugin.config_name.as_str()
                                }
                                McpServerSource::Config
                                | McpServerSource::Compatibility { .. }
                                | McpServerSource::Extension { .. } => false,
                            })
                })
                .map(|tool| tool.server_name.clone())
                .collect::<BTreeSet<String>>()
                .into_iter()
                .collect::<Vec<_>>();
            let available_apps = available_connectors
                .iter()
                .filter(|connector| {
                    connector.is_enabled
                        && connector_snapshot
                            .plugin_ids_for_connector_id(connector.id.as_str())
                            .iter()
                            .any(|plugin_id| plugin_id == &plugin.config_name)
                })
                .map(connector_display_label)
                .collect::<BTreeSet<String>>()
                .into_iter()
                .collect::<Vec<_>>();
            render_explicit_plugin_instructions(plugin, &available_mcp_servers, &available_apps)
                .map(PluginInstructions::new)
                .map(ContextualUserFragment::into)
        })
        .collect()
}

#[cfg(test)]
#[path = "injection_tests.rs"]
mod tests;
