use std::collections::HashMap;
use std::collections::HashSet;

use codex_connectors::metadata::connector_mention_slug;
use codex_protocol::user_input::UserInput;

use crate::connectors;
use crate::injection::ToolMentionKind;
use crate::injection::app_id_from_path;
use crate::injection::extract_tool_mentions_with_sigil;
use crate::injection::plugin_config_name_from_path;
use crate::injection::tool_kind_for_path;
use crate::mention_syntax::PLUGIN_TEXT_MENTION_SIGIL;
use crate::mention_syntax::TOOL_MENTION_SIGIL;

use super::PluginCapabilitySummary;

pub(crate) struct CollectedToolMentions {
    pub(crate) plain_names: HashSet<String>,
    pub(crate) paths: HashSet<String>,
}

pub(crate) fn collect_tool_mentions_from_messages(messages: &[String]) -> CollectedToolMentions {
    collect_tool_mentions_from_messages_with_sigil(messages, TOOL_MENTION_SIGIL)
}

fn collect_tool_mentions_from_messages_with_sigil(
    messages: &[String],
    sigil: char,
) -> CollectedToolMentions {
    let mut plain_names = HashSet::new();
    let mut paths = HashSet::new();
    for message in messages {
        let mentions = extract_tool_mentions_with_sigil(message, sigil);
        plain_names.extend(mentions.plain_names().map(str::to_string));
        paths.extend(mentions.paths().map(str::to_string));
    }
    CollectedToolMentions { plain_names, paths }
}

pub(crate) fn collect_explicit_app_ids(input: &[UserInput]) -> HashSet<String> {
    let mut app_ids = HashSet::new();
    let mut collect_path = |path: &str| {
        if tool_kind_for_path(path) == ToolMentionKind::App
            && let Some(app_id) = app_id_from_path(path)
        {
            app_ids.insert(app_id.to_string());
        }
    };

    for item in input {
        match item {
            UserInput::Text { text, .. } => {
                for path in extract_tool_mentions_with_sigil(text, TOOL_MENTION_SIGIL).paths() {
                    collect_path(path);
                }
            }
            UserInput::Mention { path, .. } => collect_path(path),
            _ => {}
        }
    }

    app_ids
}

/// Collect explicit structured or linked `plugin://...` mentions.
pub(crate) fn collect_explicit_plugin_mentions(
    input: &[UserInput],
    plugins: &[PluginCapabilitySummary],
) -> Vec<PluginCapabilitySummary> {
    if plugins.is_empty() {
        return Vec::new();
    }

    let mut mentioned_config_names = HashSet::new();
    let mut collect_path = |path: &str| {
        if tool_kind_for_path(path) == ToolMentionKind::Plugin
            && let Some(config_name) = plugin_config_name_from_path(path)
        {
            mentioned_config_names.insert(config_name.to_string());
        }
    };

    for item in input {
        match item {
            UserInput::Text { text, .. } => {
                // Plugin plaintext links use `@`, not the default `$` tool sigil.
                for path in
                    extract_tool_mentions_with_sigil(text, PLUGIN_TEXT_MENTION_SIGIL).paths()
                {
                    collect_path(path);
                }
            }
            UserInput::Mention { path, .. } => collect_path(path),
            _ => {}
        }
    }

    if mentioned_config_names.is_empty() {
        return Vec::new();
    }

    plugins
        .iter()
        .filter(|plugin| mentioned_config_names.contains(plugin.config_name.as_str()))
        .cloned()
        .collect()
}

pub(crate) use crate::build_skill_name_counts;

pub(crate) fn build_connector_slug_counts(
    connectors: &[connectors::AppInfo],
) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for connector in connectors {
        let slug = connector_mention_slug(connector);
        *counts.entry(slug).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
#[path = "mentions_tests.rs"]
mod tests;
