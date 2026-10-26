use std::collections::HashMap;

use codex_config::HookHandlerConfig;
use codex_config::MatcherGroup;
use codex_config::TomlValue;
use codex_config::version_for_toml;
use codex_plugin::PluginHookSource;
use codex_protocol::protocol::HookEventName;

/// Count only identical declarations, so inserting a different handler or
/// matcher group never changes the key of an existing hook.
#[derive(Default)]
pub(crate) struct HookKeyBuilder {
    occurrences: HashMap<String, usize>,
}

impl HookKeyBuilder {
    pub(crate) fn next(
        &mut self,
        key_source: &str,
        event_name: HookEventName,
        matcher: Option<&str>,
        handler: &HookHandlerConfig,
    ) -> String {
        let identity = MatcherGroup {
            matcher: crate::events::common::matcher_pattern_for_event(event_name, matcher)
                .map(str::to_owned),
            hooks: vec![handler.clone()],
        };
        #[expect(
            clippy::expect_used,
            reason = "the hook identity is a concrete TOML-compatible config schema; changing it requires a persisted-key migration"
        )]
        let identity = TomlValue::try_from(identity)
            .expect("hook declarations contain only TOML-serializable fields");
        let base = format!(
            "{key_source}:{}:v2:{}",
            crate::hook_event_key_label(event_name),
            version_for_toml(&identity),
        );
        let occurrence = self.occurrences.entry(base.clone()).or_default();
        let key = format!("{base}:{occurrence}");
        *occurrence += 1;
        key
    }
}

/// Minimal declaration metadata for one bundled plugin hook handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHookDeclaration {
    pub key: String,
    pub event_name: HookEventName,
}

/// Return the hook handlers declared by plugin bundles without projecting live runtime state.
pub fn plugin_hook_declarations(hook_sources: &[PluginHookSource]) -> Vec<PluginHookDeclaration> {
    let mut declarations = Vec::new();

    for source in hook_sources {
        let mut keys = HookKeyBuilder::default();
        let key_source = plugin_hook_key_source(
            source.plugin_id.as_key().as_str(),
            source.source_relative_path.as_str(),
        );
        for (event_name, groups) in source.hooks.clone().into_matcher_groups() {
            for group in &groups {
                for handler in &group.hooks {
                    declarations.push(PluginHookDeclaration {
                        key: keys.next(&key_source, event_name, group.matcher.as_deref(), handler),
                        event_name,
                    });
                }
            }
        }
    }

    declarations
}

pub(crate) fn plugin_hook_key_source(plugin_id: &str, source_relative_path: &str) -> String {
    format!("{plugin_id}:{source_relative_path}")
}
