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

#[cfg(test)]
mod tests {
    use codex_config::HookEventsToml;
    use codex_config::HookHandlerConfig;
    use codex_config::MatcherGroup;
    use codex_plugin::PluginId;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn identical_handlers_keep_distinct_keys_after_unrelated_insertion() {
        let root = test_path_buf("/tmp/plugin").abs();
        let mut source = PluginHookSource {
            plugin_id: PluginId::parse("demo@test").expect("plugin id"),
            plugin_root: root.clone(),
            plugin_data_root: root.join("data"),
            source_path: root.join("hooks/hooks.json"),
            source_relative_path: "hooks/hooks.json".to_string(),
            hooks: HookEventsToml {
                pre_tool_use: vec![MatcherGroup {
                    matcher: None,
                    hooks: vec![HookHandlerConfig::Prompt {}, HookHandlerConfig::Prompt {}],
                }],
                ..Default::default()
            },
        };
        let initial = plugin_hook_declarations(std::slice::from_ref(&source));
        assert_eq!(initial.len(), 2);
        assert_eq!(
            initial[0].key,
            "demo@test:hooks/hooks.json:pre_tool_use:v2:sha256:4c5e3f8c709326c100b655599cb4a2154c043113c0952a1a3ac4b3942506b183:0"
        );
        assert_eq!(
            initial[1].key,
            "demo@test:hooks/hooks.json:pre_tool_use:v2:sha256:4c5e3f8c709326c100b655599cb4a2154c043113c0952a1a3ac4b3942506b183:1"
        );
        source.hooks.pre_tool_use[0]
            .hooks
            .insert(0, HookHandlerConfig::Agent {});
        let changed = plugin_hook_declarations(&[source]);
        assert_eq!(changed.len(), 3);
        assert_eq!(&changed[1..], initial.as_slice());
    }

    #[test]
    fn lists_declared_plugin_handlers_with_persisted_hook_keys() {
        let plugin_root = test_path_buf("/tmp/plugin").abs();
        let source_path = plugin_root.join("hooks/hooks.json");
        let declarations = plugin_hook_declarations(&[PluginHookSource {
            plugin_id: PluginId::parse("demo@test").expect("plugin id"),
            plugin_root: plugin_root.clone(),
            plugin_data_root: plugin_root.join("data"),
            source_path,
            source_relative_path: "hooks/hooks.json".to_string(),
            hooks: HookEventsToml {
                pre_tool_use: vec![MatcherGroup {
                    matcher: None,
                    hooks: vec![
                        HookHandlerConfig::Prompt {},
                        HookHandlerConfig::Command {
                            command: "echo hi".to_string(),
                            command_windows: None,
                            timeout_sec: None,
                            r#async: false,
                            status_message: None,
                        },
                    ],
                }],
                session_start: vec![MatcherGroup {
                    matcher: None,
                    hooks: vec![HookHandlerConfig::Agent {}],
                }],
                ..Default::default()
            },
        }]);

        assert_eq!(
            declarations,
            vec![
                PluginHookDeclaration {
                    key: "demo@test:hooks/hooks.json:pre_tool_use:v2:sha256:4c5e3f8c709326c100b655599cb4a2154c043113c0952a1a3ac4b3942506b183:0".to_string(),
                    event_name: HookEventName::PreToolUse,
                },
                PluginHookDeclaration {
                    key: "demo@test:hooks/hooks.json:pre_tool_use:v2:sha256:fb93763db045a12fef1bd30797ac437341225a94831e8cab2528b2181f340da2:0".to_string(),
                    event_name: HookEventName::PreToolUse,
                },
                PluginHookDeclaration {
                    key: "demo@test:hooks/hooks.json:session_start:v2:sha256:6d62230c2c86a695b5170bc575c65045cf257765284ca14bed763d5e90d3f0e3:0".to_string(),
                    event_name: HookEventName::SessionStart,
                },
            ]
        );
    }
}
