use std::collections::HashMap;
use std::collections::HashSet;

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::SkillsToggleItem;
use crate::bottom_pane::SkillsToggleView;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::skills_helpers::skill_description;
use crate::skills_helpers::skill_display_name;
use codex_app_server_protocol::SkillMetadata as ProtocolSkillMetadata;
use codex_app_server_protocol::SkillsListEntry;
use codex_app_server_protocol::SkillsListResponse;
use codex_connectors::AppInfo;
use codex_core_skills::injection::ToolMentionKind;
use codex_core_skills::injection::app_id_from_path;
use codex_core_skills::injection::extract_tool_mentions;
use codex_core_skills::injection::normalize_skill_path;
use codex_core_skills::injection::tool_kind_for_path;
use codex_core_skills::model::SkillDependencies;
use codex_core_skills::model::SkillInterface;
use codex_core_skills::model::SkillMetadata;
use codex_core_skills::model::SkillToolDependency;
use codex_protocol::parse_command::ParsedCommand;
use codex_utils_absolute_path::AbsolutePathBuf;

impl ChatWidget {
    pub(crate) fn open_skills_list(&mut self) {
        self.insert_str("@");
    }

    pub(crate) fn open_skills_menu(&mut self) {
        let items = vec![
            SelectionItem {
                name: "List skills".to_string(),
                description: Some("Tip: press @ to open this list directly.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenSkillsList);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Enable/Disable Skills".to_string(),
                description: Some("Enable or disable skills.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenManageSkillsPopup);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Skills".to_string()),
            subtitle: Some("Choose an action".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn open_manage_skills_popup(&mut self) {
        if self.skills_all.is_empty() {
            self.add_info_message("No skills available.".to_string(), /*hint*/ None);
            return;
        }

        let mut initial_state = HashMap::new();
        for skill in &self.skills_all {
            initial_state.insert(skill.path.clone(), skill.enabled);
        }
        self.skills_initial_state = Some(initial_state);

        let items: Vec<SkillsToggleItem> = self
            .skills_all
            .iter()
            .filter_map(|skill| {
                let core_skill = protocol_skill_to_core(skill)?;
                let display_name = skill_display_name(&core_skill);
                let description = skill_description(&core_skill).to_string();
                let name = core_skill.name.clone();
                let path = core_skill.path_to_skills_md;
                Some(SkillsToggleItem {
                    name: display_name,
                    skill_name: name,
                    description,
                    enabled: skill.enabled,
                    path,
                })
            })
            .collect();

        let view = SkillsToggleView::new(
            items,
            self.app_event_tx.clone(),
            self.bottom_pane.list_keymap(),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    pub(crate) fn update_skill_enabled(&mut self, path: AbsolutePathBuf, enabled: bool) {
        for skill in &mut self.skills_all {
            if skill.path == path {
                skill.enabled = enabled;
            }
        }
        self.set_skills(Some(enabled_skills_for_mentions(&self.skills_all)));
    }

    pub(crate) fn handle_manage_skills_closed(&mut self) {
        let Some(initial_state) = self.skills_initial_state.take() else {
            return;
        };
        let mut current_state = HashMap::new();
        for skill in &self.skills_all {
            current_state.insert(skill.path.clone(), skill.enabled);
        }

        let mut enabled_count = 0;
        let mut disabled_count = 0;
        for (path, was_enabled) in initial_state {
            let Some(is_enabled) = current_state.get(&path) else {
                continue;
            };
            if was_enabled != *is_enabled {
                if *is_enabled {
                    enabled_count += 1;
                } else {
                    disabled_count += 1;
                }
            }
        }

        if enabled_count == 0 && disabled_count == 0 {
            return;
        }
        self.add_info_message(
            format!("{enabled_count} skills enabled, {disabled_count} skills disabled"),
            /*hint*/ None,
        );
    }

    pub(crate) fn set_skills_from_response(&mut self, response: &SkillsListResponse) {
        let skills = skills_for_cwd(&self.config.cwd, &response.data);
        self.skills_all = skills;
        self.set_skills(Some(enabled_skills_for_mentions(&self.skills_all)));
    }

    pub(crate) fn annotate_skill_reads_in_parsed_cmd(
        &self,
        mut parsed_cmd: Vec<ParsedCommand>,
    ) -> Vec<ParsedCommand> {
        if self.skills_all.is_empty() {
            return parsed_cmd;
        }

        for parsed in &mut parsed_cmd {
            let ParsedCommand::Read { name, path, .. } = parsed else {
                continue;
            };
            if name != "SKILL.md" {
                continue;
            }

            // Best effort only: annotate exact SKILL.md path matches from the loaded skills list.
            if let Some(skill) = self
                .skills_all
                .iter()
                .find(|skill| skill.path.as_path() == path)
            {
                *name = format!("{name} ({} skill)", skill.name);
            }
        }

        parsed_cmd
    }
}

fn skills_for_cwd(
    cwd: &AbsolutePathBuf,
    skills_entries: &[SkillsListEntry],
) -> Vec<ProtocolSkillMetadata> {
    skills_entries
        .iter()
        .find(|entry| entry.cwd.as_path() == cwd.as_path())
        .map(|entry| entry.skills.clone())
        .unwrap_or_default()
}

fn enabled_skills_for_mentions(skills: &[ProtocolSkillMetadata]) -> Vec<SkillMetadata> {
    skills
        .iter()
        .filter(|skill| skill.enabled)
        .filter_map(protocol_skill_to_core)
        .collect()
}

fn protocol_skill_to_core(skill: &ProtocolSkillMetadata) -> Option<SkillMetadata> {
    let scope = serde_json::to_value(skill.scope)
        .and_then(serde_json::from_value)
        .inspect_err(|err| {
            tracing::warn!(
                skill_name = %skill.name,
                %err,
                "Failed to map app-server skill scope"
            );
        })
        .ok()?;

    Some(SkillMetadata {
        name: skill.name.clone(),
        description: skill.description.clone(),
        short_description: skill.short_description.clone(),
        interface: skill.interface.clone().map(|interface| SkillInterface {
            display_name: interface.display_name,
            short_description: interface.short_description,
            icon_small: interface.icon_small,
            icon_large: interface.icon_large,
            brand_color: interface.brand_color,
            default_prompt: interface.default_prompt,
        }),
        dependencies: skill
            .dependencies
            .clone()
            .map(|dependencies| SkillDependencies {
                tools: dependencies
                    .tools
                    .into_iter()
                    .map(|tool| SkillToolDependency {
                        r#type: tool.r#type,
                        value: tool.value,
                        description: tool.description,
                        transport: tool.transport,
                        command: tool.command,
                        url: tool.url,
                    })
                    .collect(),
            }),
        policy: None,
        path_to_skills_md: skill.path.clone(),
        scope,
        plugin_id: None,
    })
}

pub(crate) fn collect_tool_mentions(
    text: &str,
    mention_paths: &HashMap<String, String>,
) -> ToolMentions {
    let parsed = extract_tool_mentions(text);
    let mut mentions = ToolMentions {
        names: parsed.names().map(str::to_string).collect(),
        linked_paths: parsed
            .linked_paths()
            .map(|(name, path)| (name.to_string(), path.to_string()))
            .collect(),
    };
    for (name, path) in mention_paths {
        if mentions.names.contains(name) {
            mentions.linked_paths.insert(name.clone(), path.clone());
        }
    }
    mentions
}

pub(crate) fn find_skill_mentions_with_tool_mentions(
    mentions: &ToolMentions,
    skills: &[SkillMetadata],
) -> Vec<SkillMetadata> {
    let mention_skill_paths: HashSet<&str> = mentions
        .linked_paths
        .values()
        .filter(|path| {
            matches!(
                tool_kind_for_path(path),
                ToolMentionKind::Skill | ToolMentionKind::Other
            )
        })
        .map(|path| normalize_skill_path(path))
        .collect();

    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut matches: Vec<SkillMetadata> = Vec::new();

    for skill in skills {
        if seen_paths.contains(&skill.path_to_skills_md) {
            continue;
        }
        let path_str = skill.path_to_skills_md.to_string_lossy();
        if mention_skill_paths.contains(path_str.as_ref()) {
            seen_paths.insert(skill.path_to_skills_md.clone());
            seen_names.insert(skill.name.clone());
            matches.push(skill.clone());
        }
    }

    for skill in skills {
        if seen_paths.contains(&skill.path_to_skills_md) {
            continue;
        }
        if mentions.names.contains(&skill.name) && seen_names.insert(skill.name.clone()) {
            seen_paths.insert(skill.path_to_skills_md.clone());
            matches.push(skill.clone());
        }
    }

    matches
}

pub(crate) fn find_app_mentions(
    mentions: &ToolMentions,
    apps: &[AppInfo],
    skill_names_lower: &HashSet<String>,
) -> Vec<AppInfo> {
    let mut explicit_names = HashSet::new();
    let mut selected_ids = HashSet::new();
    for (name, path) in &mentions.linked_paths {
        if let Some(connector_id) = app_id_from_path(path) {
            explicit_names.insert(name.clone());
            selected_ids.insert(connector_id.to_string());
        }
    }

    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    for app in apps.iter().filter(|app| is_app_mentionable(app)) {
        let slug = codex_connectors::metadata::connector_mention_slug(app);
        *slug_counts.entry(slug).or_insert(0) += 1;
    }

    for app in apps.iter().filter(|app| is_app_mentionable(app)) {
        let slug = codex_connectors::metadata::connector_mention_slug(app);
        let slug_count = slug_counts.get(&slug).copied().unwrap_or(0);
        if mentions.names.contains(&slug)
            && !explicit_names.contains(&slug)
            && slug_count == 1
            && !skill_names_lower.contains(&slug)
        {
            selected_ids.insert(app.id.clone());
        }
    }

    apps.iter()
        .filter(|app| is_app_mentionable(app) && selected_ids.contains(&app.id))
        .cloned()
        .collect()
}

pub(crate) fn is_app_mentionable(app: &AppInfo) -> bool {
    app.is_accessible && app.is_enabled
}

pub(crate) struct ToolMentions {
    names: HashSet<String>,
    linked_paths: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn app(id: &str, name: &str) -> AppInfo {
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
            plugin_display_names: Vec::new(),
        }
    }

    #[test]
    fn find_app_mentions_requires_accessible_enabled_apps_for_slugs() {
        let apps = vec![
            app("google_drive", "Google Drive"),
            AppInfo {
                is_accessible: false,
                ..app("arabica_uae", "% Arabica UAE")
            },
            AppInfo {
                is_enabled: false,
                ..app("linear", "Linear")
            },
        ];
        let mentions = collect_tool_mentions("$google-drive $arabica-uae $linear", &HashMap::new());

        assert_eq!(
            find_app_mentions(&mentions, &apps, &HashSet::new()),
            vec![apps[0].clone()]
        );
    }

    #[test]
    fn find_app_mentions_requires_accessible_enabled_apps_for_bound_paths() {
        let apps = vec![
            app("google_drive", "Google Drive"),
            AppInfo {
                is_accessible: false,
                ..app("arabica_uae", "% Arabica UAE")
            },
            AppInfo {
                is_enabled: false,
                ..app("linear", "Linear")
            },
        ];
        let mention_paths = HashMap::from([
            ("google-drive".to_string(), "app://google_drive".to_string()),
            ("arabica-uae".to_string(), "app://arabica_uae".to_string()),
            ("linear".to_string(), "app://linear".to_string()),
        ]);
        let mentions = collect_tool_mentions("$google-drive $arabica-uae $linear", &mention_paths);

        assert_eq!(
            find_app_mentions(&mentions, &apps, &HashSet::new()),
            vec![apps[0].clone()]
        );
    }
}
