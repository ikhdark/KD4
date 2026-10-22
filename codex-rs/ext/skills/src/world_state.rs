use codex_extension_api::ContextualUserFragment;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::protocol::ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_CLOSE_TAG;
use codex_protocol::protocol::SKILLS_INSTRUCTIONS_OPEN_TAG;
use serde_json::json;

use crate::catalog::SkillCatalog;
use crate::render::available_skills_fragment;

pub(crate) const SKILLS_WORLD_STATE_ID: &str = "skills";
const NO_EXECUTOR_SKILLS_BODY: &str =
    "\n## Skills update\nNo selected-environment skills are currently available.\n";
const HIDDEN_EXECUTOR_SKILLS_BODY: &str = "\n## Skills update\nSelected-environment skills are not listed automatically. Explicit skill mentions can still be resolved when available.\n";
const DEFERRED_EXECUTOR_SKILLS_BODY: &str = "\n## Skills update\nThe previous selected-environment skills catalog is no longer current. Its replacement listing was omitted due to context pressure. Discover SKILL.md files under the current selected capability roots with that environment's filesystem tools and read_file with environment_id if needed.\n";

pub(crate) fn executor_skills_world_state_section(
    catalog: &SkillCatalog,
    include_instructions: bool,
) -> WorldStateSectionContribution {
    let body = if include_instructions {
        available_skills_fragment(catalog).map(|fragment| fragment.body().into_owned())
    } else {
        None
    };
    let snapshot = json!({
        "body": body,
        "includeInstructions": include_instructions,
    });
    let retained_body = body.clone();
    let fallback_snapshot = if body.is_some() {
        json!({"includeInstructions": include_instructions, "catalogDeferred": true})
    } else {
        snapshot.clone()
    };
    let fallback_body = if body.is_some() {
        DEFERRED_EXECUTOR_SKILLS_BODY
    } else if include_instructions {
        NO_EXECUTOR_SKILLS_BODY
    } else {
        HIDDEN_EXECUTOR_SKILLS_BODY
    };

    let contribution =
        WorldStateSectionContribution::new(SKILLS_WORLD_STATE_ID, snapshot, move |previous| {
            let previous_is_absent = matches!(&previous, PreviousWorldStateSection::Absent);
            if let PreviousWorldStateSection::Known(previous) = &previous {
                let previous_body = previous.get("body").and_then(serde_json::Value::as_str);
                let previous_include_instructions = previous
                    .get("includeInstructions")
                    .and_then(serde_json::Value::as_bool);
                if previous_body == body.as_deref()
                    && previous_include_instructions == Some(include_instructions)
                    && previous
                        .get("catalogDeferred")
                        .and_then(serde_json::Value::as_bool)
                        != Some(true)
                {
                    return None;
                }
            }

            let body = match body.as_deref() {
                Some(body) => body,
                None if previous_is_absent => return None,
                None if !include_instructions => HIDDEN_EXECUTOR_SKILLS_BODY,
                None => NO_EXECUTOR_SKILLS_BODY,
            };
            Some(RenderedWorldStateFragment::new(
                "developer",
                (
                    ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG,
                    ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG,
                ),
                body,
            ))
        })
        .with_budget_fallback(
            fallback_snapshot,
            RenderedWorldStateFragment::new(
                "developer",
                (
                    ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG,
                    ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG,
                ),
                fallback_body,
            ),
        )
        .with_legacy_matcher(|role, text| {
            role == "developer"
                && ((text
                    .trim_start()
                    .starts_with(ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG)
                    && text
                        .trim_end()
                        .ends_with(ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG))
                    || (text.trim_start().starts_with(SKILLS_INSTRUCTIONS_OPEN_TAG)
                        && text.trim_end().ends_with(SKILLS_INSTRUCTIONS_CLOSE_TAG)))
        });
    match retained_body {
        Some(body) => contribution.with_retained_fragment_matcher(move |role, text| {
            role == "developer"
                && (text.contains(body.as_str()) || text.contains(DEFERRED_EXECUTOR_SKILLS_BODY))
        }),
        None => contribution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SkillAuthority;
    use crate::catalog::SkillCatalogEntry;
    use crate::catalog::SkillPackageId;
    use crate::catalog::SkillResourceId;
    use crate::catalog::SkillSourceKind;

    #[test]
    fn catalog_transitions_have_truthful_bounded_fallbacks() {
        let catalog = SkillCatalog {
            entries: vec![SkillCatalogEntry::new(
                SkillPackageId("env/new/SKILL.md".into()),
                SkillAuthority::new(SkillSourceKind::Executor, "env"),
                "replacement-skill",
                "Replacement description.",
                SkillResourceId::new("env/new/SKILL.md"),
            )],
            ..Default::default()
        };
        let previous = json!({"body": "old catalog", "includeInstructions": true});
        for (catalog, include, expected) in [
            (SkillCatalog::default(), true, NO_EXECUTOR_SKILLS_BODY),
            (catalog.clone(), false, HIDDEN_EXECUTOR_SKILLS_BODY),
            (catalog, true, DEFERRED_EXECUTOR_SKILLS_BODY),
        ] {
            let section = executor_skills_world_state_section(&catalog, include);
            let deferred = json!({"includeInstructions": true, "catalogDeferred": true});
            assert!(
                section
                    .render_diff(PreviousWorldStateSection::Known(&deferred))
                    .is_some(),
                "a deferred listing is not evidence that the current catalog is empty"
            );
            assert!(
                section
                    .render_diff(PreviousWorldStateSection::Known(&previous))
                    .is_some()
            );
            let (snapshot, fallback) = section.budget_fallback().unwrap();
            assert_eq!(fallback.role(), "developer");
            assert_eq!(fallback.body(), expected);
            assert!(
                expected.len()
                    + ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG.len()
                    + ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG.len()
                    <= 1024
            );
            if expected == DEFERRED_EXECUTOR_SKILLS_BODY {
                assert_ne!(
                    snapshot,
                    section.snapshot(),
                    "a clearing notice is not a full catalog receipt"
                );
                assert!(
                    section
                        .render_diff(PreviousWorldStateSection::Known(snapshot))
                        .is_some()
                );
                assert!(section.matches_retained_fragment("developer", expected));
            } else {
                assert_eq!(snapshot, section.snapshot());
                assert!(
                    section
                        .render_diff(PreviousWorldStateSection::Known(snapshot))
                        .is_none()
                );
            }
        }
    }
}
