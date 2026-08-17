use codex_protocol::protocol::PLUGINS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::PLUGINS_INSTRUCTIONS_OPEN_TAG;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AvailablePluginsInstructions;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PluginsInstructionsUnavailable;

impl ContextualUserFragment for AvailablePluginsInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            PLUGINS_INSTRUCTIONS_OPEN_TAG,
            PLUGINS_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        "\n## Plugins\nPlugins contribute skills (`plugin_name:skill`), MCP tools, or apps; invoke the contributed capability, not the bundle. Prefer a named plugin's relevant capabilities. If none are callable, say so briefly and use the best fallback.\n".to_string()
    }
}

impl ContextualUserFragment for PluginsInstructionsUnavailable {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            PLUGINS_INSTRUCTIONS_OPEN_TAG,
            PLUGINS_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        "\n## Plugins\nPlugins are currently unavailable. Previously provided plugin guidance no longer applies.\n"
            .to_string()
    }
}
