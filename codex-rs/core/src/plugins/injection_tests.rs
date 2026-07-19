use std::sync::Arc;

use codex_connectors::AppInfo;
use codex_connectors::PluginConnectorSource;
use codex_mcp::McpPluginAttribution;
use codex_mcp::McpServerRegistration;
use codex_mcp::codex_apps_mcp_server_config;
use codex_plugin::AppConnectorId;
use codex_protocol::models::ContentItem;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;

use super::*;

fn mcp_tool(server_name: &str, plugin_display_name: &str) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: "search".to_string(),
        callable_namespace: server_name.to_string(),
        namespace_description: None,
        tool: Tool::new_with_raw("search".to_string(), None, Arc::new(JsonObject::default())),
        connector_id: None,
        connector_name: None,
        plugin_display_names: vec![plugin_display_name.to_string()],
    }
}

fn connector(id: &str, name: &str, plugin_display_name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: true,
        plugin_display_names: vec![plugin_display_name.to_string()],
    }
}

#[test]
fn explicit_plugin_injection_uses_stable_identity_when_display_names_collide() {
    let first_id = "plugin-a@market-one";
    let second_id = "plugin-b@market-two";
    let display_name = "Shared";
    let plugins = [
        PluginCapabilitySummary {
            config_name: first_id.to_string(),
            display_name: display_name.to_string(),
            mcp_server_names: vec!["alpha-server".to_string()],
            app_connector_ids: vec![AppConnectorId("connector-alpha".to_string())],
            ..PluginCapabilitySummary::default()
        },
        PluginCapabilitySummary {
            config_name: second_id.to_string(),
            display_name: display_name.to_string(),
            mcp_server_names: vec!["beta-server".to_string()],
            app_connector_ids: vec![AppConnectorId("connector-beta".to_string())],
            ..PluginCapabilitySummary::default()
        },
    ];
    let mentioned_plugins = crate::plugins::collect_explicit_plugin_mentions(
        &[UserInput::Mention {
            name: display_name.to_string(),
            path: format!("plugin://{first_id}"),
        }],
        &plugins,
    );
    let connector_snapshot = ConnectorSnapshot::from_plugin_sources([
        PluginConnectorSource::from_connector_ids(
            first_id,
            display_name,
            [AppConnectorId("connector-alpha".to_string())],
        ),
        PluginConnectorSource::from_connector_ids(
            second_id,
            display_name,
            [AppConnectorId("connector-beta".to_string())],
        ),
    ]);
    let mut catalog = ResolvedMcpCatalog::builder();
    catalog.register(McpServerRegistration::from_plugin(
        "alpha-server".to_string(),
        McpPluginAttribution::new(first_id.to_string(), display_name.to_string()),
        /*plugin_order*/ 0,
        codex_apps_mcp_server_config(
            "https://alpha.example",
            /*apps_mcp_product_sku*/ None,
            /*originator*/ None,
        ),
    ));
    catalog.register(McpServerRegistration::from_plugin(
        "beta-server".to_string(),
        McpPluginAttribution::new(second_id.to_string(), display_name.to_string()),
        /*plugin_order*/ 1,
        codex_apps_mcp_server_config(
            "https://beta.example",
            /*apps_mcp_product_sku*/ None,
            /*originator*/ None,
        ),
    ));
    let catalog = catalog.build();

    let injections = build_plugin_injections(
        &mentioned_plugins,
        &[
            mcp_tool("alpha-server", display_name),
            mcp_tool("beta-server", display_name),
        ],
        &[
            connector("connector-alpha", "Alpha App", display_name),
            connector("connector-beta", "Beta App", display_name),
        ],
        &catalog,
        &connector_snapshot,
    );

    let [ResponseItem::Message { role, content, .. }] = injections.as_slice() else {
        panic!("expected one plugin developer message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one plugin text item");
    };
    assert_eq!(role, "developer");
    assert_eq!(
        text,
        "Capabilities from the `Shared` plugin:\n\
- MCP servers from this plugin available in this session: `alpha-server`.\n\
- Apps from this plugin available in this session: `Alpha App`.\n\
Use these plugin-associated capabilities to help solve the task."
    );
}
