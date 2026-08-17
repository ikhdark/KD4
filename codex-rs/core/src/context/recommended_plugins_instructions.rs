use super::ContextualUserFragment;
use codex_tools::DiscoverableTool;

const RECOMMENDED_PLUGINS_INTRO: &str =
    "Here is a list of plugins that are available but not installed.";
const MAX_RECOMMENDED_PLUGINS: usize = 50;
const MAX_RECOMMENDED_PLUGINS_BODY_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecommendedPluginsInstructions {
    plugins: Vec<DiscoverableTool>,
}

impl RecommendedPluginsInstructions {
    pub(crate) fn from_plugins(plugins: Vec<DiscoverableTool>) -> Option<Self> {
        let mut body_bytes = format!("\n{RECOMMENDED_PLUGINS_INTRO}\n\n\n").len();
        let plugins = plugins
            .into_iter()
            .filter(|plugin| {
                let line_bytes = plugin.name().len() + plugin.id().len() + "-  ()\n".len();
                if body_bytes.saturating_add(line_bytes) > MAX_RECOMMENDED_PLUGINS_BODY_BYTES {
                    return false;
                }
                body_bytes += line_bytes;
                true
            })
            .take(MAX_RECOMMENDED_PLUGINS)
            .collect::<Vec<_>>();
        (!plugins.is_empty()).then_some(Self { plugins })
    }
}

impl ContextualUserFragment for RecommendedPluginsInstructions {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<recommended_plugins>", "</recommended_plugins>")
    }

    fn body(&self) -> String {
        let plugins = self
            .plugins
            .iter()
            .map(|plugin| format!("- {} ({})", plugin.name(), plugin.id()))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n{RECOMMENDED_PLUGINS_INTRO}\n\n{plugins}\n")
    }
}
