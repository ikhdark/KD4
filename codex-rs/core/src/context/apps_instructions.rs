use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::protocol::APPS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppsInstructions;

impl ContextualUserFragment for AppsInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (APPS_INSTRUCTIONS_OPEN_TAG, APPS_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> String {
        format!(
            "\n## Apps (Connectors)\nApps expose MCP tools through the `{CODEX_APPS_MCP_SERVER_NAME}` MCP. Use an installed app when the user names it with `[$app-name](app://{{connector_id}})` or when it is clearly relevant to the request.\nAn app's tools may already be available or may be discoverable through `tool_search`; use that tool only when it is listed for the current turn.\nDo not use `list_mcp_resources` or `list_mcp_resource_templates` to discover apps.\n"
        )
    }
}
