use std::collections::HashSet;

use codex_core_skills::injection::ToolMentionKind;
use codex_core_skills::injection::extract_tool_mentions;
use codex_core_skills::injection::normalize_skill_path;
use codex_core_skills::injection::tool_kind_for_path;
use codex_protocol::user_input::UserInput;

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(
        input_count = inputs.len(),
        catalog_entry_count = catalog.entries.len()
    )
)]
pub(crate) fn collect_explicit_skill_mentions(
    inputs: &[UserInput],
    catalog: &SkillCatalog,
) -> Vec<SkillCatalogEntry> {
    let catalog = CanonicalSkillCatalog::new(catalog);
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut blocked_plain_names = HashSet::new();

    for input in inputs {
        match input {
            UserInput::Skill { name, path } => {
                blocked_plain_names.insert(name.clone());
                let path = path.to_string_lossy();
                select_by_path(
                    &catalog,
                    CanonicalSkillPath::new(&path),
                    &mut seen,
                    &mut selected,
                );
            }
            UserInput::Mention { name, path } => {
                if let Some(path) = CanonicalSkillPath::from_mention(path) {
                    blocked_plain_names.insert(name.clone());
                    select_by_path(&catalog, path, &mut seen, &mut selected);
                }
            }
            UserInput::Text { .. } | UserInput::Image { .. } | UserInput::LocalImage { .. } => {}
            _ => {}
        }
    }

    for input in inputs {
        let UserInput::Text { text, .. } = input else {
            continue;
        };

        let mentions = extract_tool_mentions(text);
        for path in mentions.paths() {
            if let Some(path) = CanonicalSkillPath::from_mention(path) {
                select_by_path(&catalog, path, &mut seen, &mut selected);
            }
        }
        for name in mentions.plain_names() {
            if blocked_plain_names.contains(name) {
                continue;
            }
            if let Some(entry) = catalog
                .entries
                .iter()
                .rev()
                .find(|entry| entry.entry.name == name)
            {
                push_selected(entry.entry, &mut seen, &mut selected);
            }
        }
    }

    selected
}

fn select_by_path(
    catalog: &CanonicalSkillCatalog<'_>,
    path: CanonicalSkillPath<'_>,
    seen: &mut HashSet<SkillCatalogEntryKey>,
    selected: &mut Vec<SkillCatalogEntry>,
) {
    for entry in &catalog.entries {
        if entry.matches(path) {
            push_selected(entry.entry, seen, selected);
        }
    }
}

fn push_selected(
    entry: &SkillCatalogEntry,
    seen: &mut HashSet<SkillCatalogEntryKey>,
    selected: &mut Vec<SkillCatalogEntry>,
) {
    let key = SkillCatalogEntryKey::from(entry);
    if seen.insert(key) {
        selected.push(entry.clone());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CanonicalSkillPath<'a>(&'a str);

impl<'a> CanonicalSkillPath<'a> {
    fn new(path: &'a str) -> Self {
        Self(normalize_skill_path(path))
    }

    fn from_mention(path: &'a str) -> Option<Self> {
        (tool_kind_for_path(path) == ToolMentionKind::Skill).then(|| Self::new(path))
    }
}

struct CanonicalSkillCatalog<'a> {
    entries: Vec<CanonicalSkillCatalogEntry<'a>>,
}

impl<'a> CanonicalSkillCatalog<'a> {
    fn new(catalog: &'a SkillCatalog) -> Self {
        Self {
            entries: catalog
                .entries
                .iter()
                .filter(|entry| entry.enabled)
                .map(CanonicalSkillCatalogEntry::new)
                .collect(),
        }
    }
}

struct CanonicalSkillCatalogEntry<'a> {
    entry: &'a SkillCatalogEntry,
    main_prompt: CanonicalSkillPath<'a>,
    id: CanonicalSkillPath<'a>,
    display_path: Option<CanonicalSkillPath<'a>>,
}

impl<'a> CanonicalSkillCatalogEntry<'a> {
    fn new(entry: &'a SkillCatalogEntry) -> Self {
        Self {
            entry,
            main_prompt: CanonicalSkillPath::new(entry.main_prompt.as_str()),
            id: CanonicalSkillPath::new(&entry.id.0),
            display_path: entry.display_path.as_deref().map(CanonicalSkillPath::new),
        }
    }

    fn matches(&self, path: CanonicalSkillPath<'_>) -> bool {
        self.main_prompt.0 == path.0
            || self.id.0 == path.0
            || self
                .display_path
                .is_some_and(|candidate| candidate.0 == path.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SkillCatalogEntryKey {
    authority: SkillAuthority,
    package: SkillPackageId,
}

impl From<&SkillCatalogEntry> for SkillCatalogEntryKey {
    fn from(entry: &SkillCatalogEntry) -> Self {
        Self {
            authority: entry.authority.clone(),
            package: entry.id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_skill_path_classifies_and_normalizes_once_at_the_boundary() {
        assert_eq!(
            CanonicalSkillPath::from_mention("skill://root/demo/SKILL.md"),
            Some(CanonicalSkillPath("root/demo/SKILL.md"))
        );
        assert_eq!(
            CanonicalSkillPath::from_mention("C:\\skills\\demo\\skill.md"),
            Some(CanonicalSkillPath("C:\\skills\\demo\\skill.md"))
        );
        assert_eq!(CanonicalSkillPath::from_mention("app://demo"), None);
    }
}
