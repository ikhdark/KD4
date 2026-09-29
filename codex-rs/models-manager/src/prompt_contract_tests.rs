use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;

use crate::prompt_resolver::LOCAL_PROMPT_POLICY_SLUGS;

#[test]
fn local_policy_models_use_one_canonical_prompt() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let prompts = LOCAL_PROMPT_POLICY_SLUGS
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
fn bundled_local_policy_catalog_defers_prompt_to_local_policy() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../models.json"))
        .expect("bundled models.json should parse");
    let models = catalog["models"]
        .as_array()
        .expect("bundled models.json should contain a models array");

    for slug in LOCAL_PROMPT_POLICY_SLUGS {
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
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let bundled_slugs = response
        .models
        .iter()
        .map(|model| model.slug.as_str())
        .filter(|slug| {
            matches!(
                *slug,
                "gpt-6-astra"
                    | "gpt-6-sol"
                    | "gpt-6-luna"
                    | "gpt-5.5"
                    | "gpt-5.4"
                    | "gpt-5.4-mini"
                    | "gpt-5.2"
            ) || slug.starts_with("gpt-5.6-")
        })
        .collect::<BTreeSet<_>>();
    let registered_slugs = LOCAL_PROMPT_POLICY_SLUGS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    assert_eq!(bundled_slugs, registered_slugs);
}

#[test]
fn behavior_identical_instruction_templates_are_removed() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for slug in LOCAL_PROMPT_POLICY_SLUGS.iter().copied() {
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
