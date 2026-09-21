use std::borrow::Cow;

use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::fragments::AvailableSkillsInstructions;

const MAX_AVAILABLE_SKILLS_BYTES: usize = 8_000;
pub(crate) const MAX_SELECTED_PROMPT_BYTES: usize = 32_000;
const MAX_CATALOG_SKILL_DESCRIPTION_CHARS: usize = 1_024;
const TRUNCATED_SKILL_DESCRIPTION_SUFFIX: &str = "...";

#[tracing::instrument(
    level = "trace",
    skip_all,
    fields(catalog_entry_count = catalog.entries.len())
)]
pub(crate) fn available_skills_fragment(
    catalog: &SkillCatalog,
) -> Option<AvailableSkillsInstructions> {
    // Reserve space for bounded omission and recovery guidance.
    let mut total_bytes = 1_024usize;
    let mut omitted = 0usize;
    let mut skill_lines = Vec::new();

    for entry in catalog
        .entries
        .iter()
        .filter(|entry| entry.enabled && entry.prompt_visible)
    {
        let description = entry
            .short_description
            .as_deref()
            .unwrap_or(entry.description.as_str());
        let description = truncate_catalog_skill_description(description);
        let locator_kind = match &entry.authority.kind {
            SkillSourceKind::Host => "file",
            SkillSourceKind::Executor => "environment resource",
            SkillSourceKind::Orchestrator => "orchestrator resource",
            SkillSourceKind::Custom(_) => "custom resource",
        };
        let line_bytes = entry
            .name
            .len()
            .saturating_add(entry.rendered_path().len())
            .saturating_add(locator_kind.len())
            .saturating_add(description.len())
            .saturating_add(if description.is_empty() { 8 } else { 9 });
        let next_bytes = total_bytes.saturating_add(line_bytes);
        if next_bytes > MAX_AVAILABLE_SKILLS_BYTES {
            omitted = omitted.saturating_add(1);
            continue;
        }
        total_bytes = next_bytes;
        skill_lines.push(render_skill_line(entry, description.as_ref(), locator_kind));
    }

    if skill_lines.is_empty() && omitted == 0 {
        return None;
    }
    if omitted > 0 {
        let skill_word = if omitted == 1 { "skill" } else { "skills" };
        skill_lines.push(format!(
            "- {omitted} additional {skill_word} omitted from this bounded skills list."
        ));
    }
    if catalog.entries.iter().any(|entry| {
        entry.enabled
            && entry.prompt_visible
            && entry.authority.kind == SkillSourceKind::Orchestrator
    }) {
        skill_lines.push("- Recover orchestrator skills with skills.list({\"authority\":{\"kind\":\"orchestrator\"}}); follow next_cursor as cursor, then pass the returned authority, package, and main_resource as resource to skills.read.".to_string());
    }
    if omitted > 0
        && catalog.entries.iter().any(|entry| {
            entry.enabled && entry.prompt_visible && entry.authority.kind == SkillSourceKind::Host
        })
    {
        skill_lines.push("- For host skills, use the host skills catalog's intact skill: locators with read_file, or discover SKILL.md files under the configured skill roots with filesystem tools.".to_string());
    }
    if omitted > 0
        && catalog.entries.iter().any(|entry| {
            entry.enabled
                && entry.prompt_visible
                && entry.authority.kind == SkillSourceKind::Executor
        })
    {
        skill_lines.push("- For environment skills, discover SKILL.md files under the selected capability roots using that environment's filesystem tools; read them with read_file and its environment_id.".to_string());
    }

    Some(AvailableSkillsInstructions::from_skill_lines(skill_lines))
}

pub(crate) fn truncate_catalog_skill_description(description: &str) -> Cow<'_, str> {
    if description
        .char_indices()
        .nth(MAX_CATALOG_SKILL_DESCRIPTION_CHARS)
        .is_none()
    {
        return Cow::Borrowed(description);
    }

    let prefix_chars = MAX_CATALOG_SKILL_DESCRIPTION_CHARS
        .saturating_sub(TRUNCATED_SKILL_DESCRIPTION_SUFFIX.chars().count());
    let prefix_end = description
        .char_indices()
        .nth(prefix_chars)
        .map_or(description.len(), |(index, _)| index);
    let mut truncated = description[..prefix_end].to_string();
    truncated.push_str(TRUNCATED_SKILL_DESCRIPTION_SUFFIX);
    Cow::Owned(truncated)
}

fn render_skill_line(entry: &SkillCatalogEntry, description: &str, locator_kind: &str) -> String {
    let name = entry.name.as_str();
    let path = entry.rendered_path();
    if description.is_empty() {
        format!("- {name}: ({locator_kind}: {path})")
    } else {
        format!("- {name}: {description} ({locator_kind}: {path})")
    }
}

pub(crate) fn main_prompt_fragment(
    contents: &str,
    entry: &SkillCatalogEntry,
    budget: usize,
) -> Option<(crate::fragments::SkillInstructions, bool)> {
    use codex_extension_api::ContextualUserFragment;
    let fragment = |contents| crate::fragments::SkillInstructions {
        name: entry.name.clone(),
        path: entry.main_prompt.as_str().to_string(),
        contents,
        source_scope: entry.source_scope,
    };
    if contents.len() <= budget {
        let complete = fragment(contents.to_string());
        if complete.render().len() <= budget {
            return Some((complete, false));
        }
    }
    let fingerprint = crate::tools::read::value_fingerprint(contents);
    let partial = |end: usize| {
        let recovery = match &entry.authority.kind {
            SkillSourceKind::Orchestrator => format!(
                "Read the remaining instructions with skills.read({}); follow next_cursor as cursor until absent.",
                serde_json::json!({"authority":{"kind":"orchestrator"}, "package":entry.id.0,
                    "resource":entry.main_prompt.as_str(), "cursor":format!("{fingerprint:016x}:{end}")})
            ),
            SkillSourceKind::Host => format!("Read the full instructions from the host file {} using filesystem tools.", serde_json::json!(entry.main_prompt.as_str())),
            SkillSourceKind::Executor => match entry.main_prompt.environment_path() {
                Some((environment_id, path)) => format!("Read the full instructions with read_file({}); follow its artifact continuation.", serde_json::json!({"environment_id":environment_id,"path":path.inferred_native_path_string()})),
                None => "Report that the resource has no environment binding and its instructions are unavailable.".to_string(),
            },
            SkillSourceKind::Custom(_) => "Report missing instructions; this provider exposes no model-callable recovery route.".to_string(),
        };
        fragment(format!(
            "{}\n\n[This skill's instructions are incomplete. The omitted portion has not been loaded. {recovery}]",
            &contents[..end]
        ))
    };
    let empty = partial(0);
    if empty.render().len() > budget {
        return None;
    }
    let mut best = empty;
    let mut low = 0;
    let mut high = contents.floor_char_boundary(contents.len().min(budget));
    while low < high {
        let end = contents.ceil_char_boundary(low.midpoint(high) + 1);
        let candidate = partial(end);
        if candidate.render().len() <= budget {
            low = end;
            best = candidate;
        } else {
            high = contents.floor_char_boundary(end - 1);
        }
    }
    Some((best, true))
}

pub(crate) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let truncated = &contents[..contents.floor_char_boundary(max_bytes)];
    (truncated.to_string(), truncated.len() < contents.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_instructions_share_a_rendered_budget_and_small_skills_are_complete() {
        use codex_extension_api::ContextualUserFragment;
        let entry = SkillCatalogEntry::new(
            crate::catalog::SkillPackageId("skill://test/demo".to_string()),
            crate::catalog::SkillAuthority::new(SkillSourceKind::Orchestrator, "codex_apps"),
            "demo",
            "description",
            crate::catalog::SkillResourceId::new("skill://test/demo/SKILL.md"),
        );
        let text = "x".repeat(8_001);
        let (complete, partial) =
            main_prompt_fragment(&text, &entry, MAX_SELECTED_PROMPT_BYTES).expect("fragment");
        assert!(!partial);
        assert_eq!(complete.contents, text);
        let mut remaining = MAX_SELECTED_PROMPT_BYTES;
        let mut rendered = 0;
        for index in 0..5 {
            let budget = remaining / (5 - index);
            let (fragment, partial) = main_prompt_fragment(&"<&>🚀".repeat(2_000), &entry, budget)
                .expect("partial instructions");
            assert!(partial);
            assert!(fragment.render().len() <= budget);
            assert!(fragment.contents.contains("\"cursor\":"));
            rendered += fragment.render().len();
            remaining -= fragment.render().len();
        }
        assert!(rendered <= MAX_SELECTED_PROMPT_BYTES);
    }

    #[test]
    fn truncate_utf8_to_bytes_stops_before_split_character() {
        assert_eq!(truncate_utf8_to_bytes("a😀z", 4), ("a".to_string(), true));
    }
}
