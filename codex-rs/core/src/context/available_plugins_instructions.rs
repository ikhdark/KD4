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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(
            "\n## Plugins\nPlugins contribute skills (`plugin_name:skill`), MCP tools, or apps; use the contributed capability, not the bundle. Prefer a named plugin's relevant capability, loading or discovering it through its existing route when needed. If unavailable, explain the limitation; use a fallback only if it preserves the requested source and scope.\n",
        )
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

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(
            "\n## Plugins\nPlugins are currently unavailable. Previously provided plugin guidance no longer applies.\n",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn renders_capability_routing_and_source_preserving_fallback_guidance() {
        let expected_body = "\n## Plugins\nPlugins contribute skills (`plugin_name:skill`), MCP tools, or apps; use the contributed capability, not the bundle. Prefer a named plugin's relevant capability, loading or discovering it through its existing route when needed. If unavailable, explain the limitation; use a fallback only if it preserves the requested source and scope.\n";
        assert_eq!(AvailablePluginsInstructions.body(), expected_body);
        assert_eq!(AvailablePluginsInstructions.role(), "developer");
        assert_eq!(
            AvailablePluginsInstructions.render(),
            format!("<plugins_instructions>{expected_body}</plugins_instructions>")
        );
    }
}
