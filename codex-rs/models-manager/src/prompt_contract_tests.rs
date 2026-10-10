use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

use crate::prompt_resolver::LOCAL_PROMPT_POLICY_SLUGS;

// Legacy models retain their prompt policy without being added to the catalog.
const BUNDLED_LOCAL_POLICY_SLUGS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

#[test]
fn local_policy_models_use_one_canonical_prompt() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let prompts = BUNDLED_LOCAL_POLICY_SLUGS
        .iter()
        .map(|slug| {
            response
                .models
                .iter()
                .find(|model| model.slug == *slug)
                .unwrap_or_else(|| panic!("bundled models should contain {slug}"))
                .base_instructions
                .as_str()
        })
        .collect::<Vec<_>>();

    assert!(prompts.iter().all(|prompt| *prompt == prompts[0]));
    assert_eq!(prompts[0], BASE_INSTRUCTIONS_DEFAULT.trim());
}

#[test]
fn evidence_guidance_reaches_local_policy_and_fallback_models() {
    let mut response = crate::bundled_models_response().expect("bundled models.json should parse");
    response
        .models
        .retain(|model| BUNDLED_LOCAL_POLICY_SLUGS.contains(&model.slug.as_str()));
    response
        .models
        .push(crate::model_info::model_info_from_slug("unknown-model"));

    for model in response.models {
        let instructions = model.get_model_instructions(None);
        for required in [
            "Direct file reads establish exact content at that time; discovery-only search hits identify candidates.",
            "Complete search results establish exact matching facts within their recorded scope and snapshot, not omitted context or broader behavior.",
            "Preserve source and freshness through summaries and durable state.",
            "Storage or repetition never upgrades evidence strength.",
            "Treat generated summaries as derived and potentially lossy, cached observations as potentially stale, and inferred relationships as hypotheses.",
            "Use provenance labels such as `direct_file_read`, `search_hit`, `generated_summary`, `cached_observation`, `inferred_relationship`, and `test_result` only when they preserve a material distinction.",
            "These labels are optional internal aids, not a required user-facing reporting format.",
            "Before citing exact values (versions, names, counts, paths, subcommands), check retained evidence; if unavailable or stale, refresh it or mark the value unknown.",
            "Never substitute a remembered value while citing an earlier read.",
            "Resolve contradictions using runtime reachability, ownership, freshness, and generated-source contracts; revise conclusions when evidence disagrees and never fill an unknown with an unstated assumption.",
        ] {
            assert!(
                instructions.contains(required),
                "{} is missing evidence guidance: {required}",
                model.slug
            );
        }
    }
}

#[test]
fn bundled_local_policy_catalog_defers_prompt_to_local_policy() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../models.json"))
        .expect("bundled models.json should parse");
    let models = catalog["models"]
        .as_array()
        .expect("bundled models.json should contain a models array");

    for slug in BUNDLED_LOCAL_POLICY_SLUGS {
        let model = models
            .iter()
            .find(|model| model["slug"].as_str() == Some(slug))
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"));

        assert_eq!(
            model["base_instructions"].as_str(),
            Some(""),
            "{slug} should defer prompt content to its registered local policy"
        );
    }
}

#[test]
fn bundled_local_policy_models_match_prompt_policy_registration() {
    for slug in BUNDLED_LOCAL_POLICY_SLUGS {
        assert!(LOCAL_PROMPT_POLICY_SLUGS.contains(slug));
    }
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let bundled_slugs = response
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .filter(|slug| {
            crate::prompt_resolver::resolve_prompt(slug, None).source
                == crate::prompt_resolver::PromptSource::LocalModelPolicy
        })
        .collect::<BTreeSet<_>>();
    let registered_slugs = BUNDLED_LOCAL_POLICY_SLUGS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    assert_eq!(bundled_slugs, registered_slugs);
}

#[test]
fn behavior_identical_instruction_templates_are_removed() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for slug in BUNDLED_LOCAL_POLICY_SLUGS.iter().copied() {
        let model = response
            .models
            .iter()
            .find(|model| model.slug == slug)
            .unwrap_or_else(|| panic!("bundled models.json should contain {slug}"));
        assert!(
            model
                .model_messages
                .as_ref()
                .is_none_or(|messages| messages.instructions_template.is_none()),
            "{slug} should not duplicate base_instructions in instructions_template"
        );
        assert_eq!(model.get_model_instructions(None), model.base_instructions);
        for personality in [
            Personality::None,
            Personality::Friendly,
            Personality::Pragmatic,
        ] {
            assert_eq!(
                model.get_model_instructions(Some(personality)),
                model.base_instructions,
                "{slug} should preserve base rendering for {personality}"
            );
        }
        assert!(!model.supports_personality());
    }
}
