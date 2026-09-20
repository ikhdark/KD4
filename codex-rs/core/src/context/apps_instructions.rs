use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::protocol::APPS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::APPS_INSTRUCTIONS_OPEN_TAG;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppsInstructions;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppsInstructionsUnavailable;

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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Owned(format!(
            "\n## Apps (Connectors)\nUse a relevant installed app when named as `[$app-name](app://{{connector_id}})` or clearly matched by the task. Use the app's available `{CODEX_APPS_MCP_SERVER_NAME}` tools directly. Use `tool_search`, when available, only to discover missing tools needed for the task. If the required tools remain unavailable, explain the limitation. Do not discover apps through MCP resource-listing tools.\n"
        ))
    }
}

impl ContextualUserFragment for AppsInstructionsUnavailable {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (APPS_INSTRUCTIONS_OPEN_TAG, APPS_INSTRUCTIONS_CLOSE_TAG)
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(
            "\n## Apps (Connectors)\nApps are currently unavailable. Previously provided Apps guidance no longer applies.\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn renders_direct_tool_use_before_discovery_guidance() {
        let expected_body = "\n## Apps (Connectors)\nUse a relevant installed app when named as `[$app-name](app://{connector_id})` or clearly matched by the task. Use the app's available `codex_apps` tools directly. Use `tool_search`, when available, only to discover missing tools needed for the task. If the required tools remain unavailable, explain the limitation. Do not discover apps through MCP resource-listing tools.\n";
        assert_eq!(AppsInstructions.body(), expected_body);
        assert_eq!(AppsInstructions.role(), "developer");
        assert_eq!(
            AppsInstructions.render(),
            format!("<apps_instructions>{expected_body}</apps_instructions>")
        );
    }
}
