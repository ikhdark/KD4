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
            "\n## Apps (Connectors)\nUse a relevant installed app when named as `[$app-name](app://{{connector_id}})` or clearly matched by the task. Its `{CODEX_APPS_MCP_SERVER_NAME}` tools are either present or discoverable through `tool_search` when that tool is available. Do not discover apps through MCP resource-listing tools.\n"
        )
    }
}
