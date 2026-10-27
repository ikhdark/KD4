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
pub(crate) const SKILLS_DISCOVERY_WORLD_STATE_ID: &str = "skills_discovery";
const INCOMPLETE_EXECUTOR_SKILLS_BODY: &str = "\n## Skills discovery update\nSelected-environment skill discovery is incomplete. Earlier accepted catalog entries are historical, not confirmed current; this observation does not establish that skills are absent. Any entries listed below are only the known partial catalog. Discover SKILL.md files under the selected capability roots with that environment's filesystem tools and read_file with environment_id if needed.\n";
const NO_EXECUTOR_SKILLS_BODY: &str =
    "\n## Skills update\nNo selected-environment skills are currently available.\n";
const HIDDEN_EXECUTOR_SKILLS_BODY: &str = "\n## Skills update\nSelected-environment skills are not listed automatically. Explicit skill mentions can still be resolved when available.\n";
const DEFERRED_EXECUTOR_SKILLS_BODY: &str = "\n## Skills update\nThe previous selected-environment skills catalog is no longer current. Its replacement listing was omitted due to context pressure. Discover SKILL.md files under the current selected capability roots with that environment's filesystem tools and read_file with environment_id if needed.\n";

pub(crate) fn executor_skills_world_state_sections(
    catalog: &SkillCatalog,
    include_instructions: bool,
) -> Vec<WorldStateSectionContribution> {
    // Discovery/read/metadata warnings cannot prove absence. Retain the accepted catalog,
    // but separately tell the model it is not a confirmed current inventory.
    let incomplete = include_instructions && !catalog.warnings.is_empty();
    let mut sections = vec![if incomplete {
        WorldStateSectionContribution::unavailable(SKILLS_WORLD_STATE_ID)
    } else {
        executor_skills_world_state_section(catalog, include_instructions)
    }];
    let status = if incomplete {
        "incomplete"
    } else if include_instructions {
        "complete"
    } else {
        "hidden"
    };
    let short_body = if incomplete {
        INCOMPLETE_EXECUTOR_SKILLS_BODY
    } else if include_instructions {
        "\n## Skills discovery update\nSelected-environment skill discovery completed. The earlier incomplete-discovery notice no longer applies; use the current skills listing.\n"
    } else {
        "\n## Skills discovery update\nThe earlier discovery notice no longer applies. Selected-environment skills are not listed automatically; this does not disable them.\n"
    };
    let mut body = short_body.to_string();
    if incomplete && let Some(fragment) = available_skills_fragment(catalog) {
        body.push_str(fragment.body().as_ref());
    }
    let snapshot = json!({"status": status, "body": body});
    let current_snapshot = snapshot.clone();
    let retained_body = body.clone();
    let mut discovery = WorldStateSectionContribution::new(
        SKILLS_DISCOVERY_WORLD_STATE_ID,
        snapshot,
        move |previous| {
            if matches!(&previous, PreviousWorldStateSection::Known(previous) if *previous == &current_snapshot)
                || (!incomplete
                    && match &previous {
                        PreviousWorldStateSection::Absent => true,
                        PreviousWorldStateSection::Known(previous) =>
                            previous.get("status").and_then(serde_json::Value::as_str) != Some("incomplete"),
                        PreviousWorldStateSection::Unknown => false,
                    })
            {
                return None;
            }
            Some(RenderedWorldStateFragment::new(
                "developer",
                ("<environment_skills_discovery>", "</environment_skills_discovery>"),
                body.clone(),
            ))
        },
    )
    .with_budget_fallback(
        json!({"status": status, "body": short_body}),
        RenderedWorldStateFragment::new(
            "developer",
            ("<environment_skills_discovery>", "</environment_skills_discovery>"),
            short_body,
        ),
    );
    if incomplete {
        discovery = discovery.with_retained_fragment_matcher(move |role, text| {
            role == "developer" && (text.contains(&retained_body) || text.contains(short_body))
        });
    }
    sections.push(discovery);
    sections
}

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
