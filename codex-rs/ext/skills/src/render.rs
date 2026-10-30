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
        let line = render_skill_line(entry, description.as_ref(), locator_kind);
        let line_bytes = line.len().saturating_add(1);
        let next_bytes = total_bytes.saturating_add(line_bytes);
        if next_bytes > MAX_AVAILABLE_SKILLS_BYTES {
            omitted = omitted.saturating_add(1);
            continue;
        }
        total_bytes = next_bytes;
        skill_lines.push(line);
    }

    let incomplete = catalog.continuation.is_some();
    if skill_lines.is_empty() && omitted == 0 && !incomplete {
        return None;
    }
    if catalog.discovery_blocked() {
        skill_lines.push("- Orchestrator skill discovery stopped on repeated pagination. Known skills remain available; use skills.list and follow next_cursor to retry incomplete discovery.".to_string());
    } else if incomplete {
        skill_lines.push("- Orchestrator skill discovery is incomplete; undiscovered skills may still be available. Continue discovery only if needed for this task.".to_string());
    }
    if omitted > 0 {
        let skill_word = if omitted == 1 { "skill" } else { "skills" };
        skill_lines.push(format!(
            "- {omitted} additional {skill_word} omitted from this bounded skills list."
        ));
    }
    if incomplete
        || catalog.entries.iter().any(|entry| {
            entry.enabled
                && entry.prompt_visible
                && entry.authority.kind == SkillSourceKind::Orchestrator
        })
    {
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
    let empty = description.is_empty();
    let name = codex_utils_string::xml_text(&entry.name);
    let description = codex_utils_string::xml_text(description);
    let path = entry.rendered_path();
    let path = codex_utils_string::xml_text(path);
    if empty {
        format!("- {name}: ({locator_kind}: {path})")
    } else {
        format!("- {name}: {description} ({locator_kind}: {path})")
    }
}

/// Water-fill the shared rendered-byte cap after every complete cost is known.
/// Small bodies keep their unused share available to large bodies, independent
/// of catalog order. Recovery envelopes remain accounted by the renderer.
pub(crate) fn selected_prompt_budgets(costs: &[usize], cap: usize) -> Vec<usize> {
    let mut order = (0..costs.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| (costs[*index], *index));
    let mut budgets = vec![0; costs.len()];
    let mut remaining = cap;
    for (position, index) in order.into_iter().enumerate() {
        let budget = costs[index].min(remaining / (costs.len() - position));
        budgets[index] = budget;
        remaining -= budget;
    }
    budgets
}

pub(crate) fn main_prompt_fragment(
    contents: &str,
    entry: &SkillCatalogEntry,
    budget: usize,
) -> Option<(crate::fragments::SkillInstructions, bool)> {
    let fragment = |contents| crate::fragments::SkillInstructions {
        name: entry.name.clone(),
        path: entry.main_prompt.as_str().to_string(),
        contents,
        source_scope: entry.source_scope,
    };
    let rendered_len = |parts: &[&str]| crate::fragments::SkillInstructions::rendered_len_parts(
        &entry.name, entry.main_prompt.as_str(), parts, entry.source_scope,
    );
    if contents.len() <= budget && rendered_len(&[contents]) <= budget {
        return Some((fragment(contents.to_string()), false));
    }
    let fingerprint = crate::tools::read::value_fingerprint(contents);
    let suffix = |end: usize| {
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
        format!("\n\n[This skill's instructions are incomplete. The omitted portion has not been loaded. {recovery}]")
    };
    if rendered_len(&[&suffix(0)]) > budget {
        return None;
    }
    let mut low = 0;
    let mut high = contents.floor_char_boundary(contents.len().min(budget));
    while low < high {
        let end = contents.ceil_char_boundary(low.midpoint(high) + 1);
        if rendered_len(&[&contents[..end], &suffix(end)]) <= budget {
            low = end;
        } else {
            high = contents.floor_char_boundary(end - 1);
        }
    }
    Some((fragment(format!("{}{}", &contents[..low], suffix(low))), true))
}

pub(crate) fn truncate_utf8_to_bytes(contents: &str, max_bytes: usize) -> (String, bool) {
    let truncated = &contents[..contents.floor_char_boundary(max_bytes)];
    (truncated.to_string(), truncated.len() < contents.len())
}
