use std::borrow::Cow;

use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillSourceKind;
use crate::fragments::AvailableSkillsInstructions;

const MAX_AVAILABLE_SKILLS_BYTES: usize = 8_000;
const MAX_MAIN_PROMPT_BYTES: usize = 8_000;
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

pub(crate) fn truncate_main_prompt_contents(
    contents: &str,
    entry: &SkillCatalogEntry,
) -> (String, bool) {
    if contents.len() <= MAX_MAIN_PROMPT_BYTES {
        return (contents.to_string(), false);
    }
    let recovery = match &entry.authority.kind {
        SkillSourceKind::Orchestrator => format!(
            "Read the full instructions with skills.read({}); follow next_cursor as cursor until absent.",
            serde_json::json!({
                "authority": {"kind": "orchestrator"},
                "package": entry.id.0,
                "resource": entry.main_prompt.as_str(),
            })
        ),
        SkillSourceKind::Host => format!(
            "Read the full instructions from the host file {} using filesystem tools.",
            serde_json::json!(entry.main_prompt.as_str())
        ),
        SkillSourceKind::Executor => match entry.main_prompt.environment_path() {
            Some((environment_id, path)) => format!(
                "Read the full instructions with read_file({}); follow the returned artifact continuation for remaining text.",
                serde_json::json!({"environment_id": environment_id, "path": path.inferred_native_path_string()})
            ),
            None => "The resource has no environment binding; report that its full instructions are unavailable.".to_string(),
        },
        SkillSourceKind::Custom(_) => "No model-callable read route is exposed for this custom provider; report the missing instructions.".to_string(),
    };
    let notice = format!(
        "\n\n[This skill's instructions are incomplete because the context limit was reached. The omitted portion has not been loaded. {recovery}]"
    );
    let notice = if notice.len() <= MAX_MAIN_PROMPT_BYTES {
        notice
    } else {
        "\n\n[This skill's instructions are incomplete. Its recovery identity exceeds the context budget; do not use a shortened identity. Report the unavailable instructions.]".to_string()
    };
    let (mut prefix, _) = truncate_utf8_to_bytes(contents, MAX_MAIN_PROMPT_BYTES - notice.len());
    prefix.push_str(&notice);
    (prefix, true)
}

pub(crate) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let truncated = &contents[..contents.floor_char_boundary(max_bytes)];
    (truncated.to_string(), truncated.len() < contents.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_to_bytes_stops_before_split_character() {
        assert_eq!(truncate_utf8_to_bytes("a😀z", 4), ("a".to_string(), true));
    }
}
