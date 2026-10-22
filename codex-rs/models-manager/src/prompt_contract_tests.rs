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
            "Separate what you observed from what you infer, and check an exact value (version, name, count, path) against what you observed in this session before citing it, or mark it unknown.",
            "A search hit is a candidate; an empty search proves absence only within its scope.",
            "Do not attach freshness or provenance caveats to an answer unless that evidence is actually in doubt.",
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

#[test]
fn catalog_guidance_fields_have_typed_owners() {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../models.json")).unwrap();
    for model in catalog["models"].as_array().unwrap() {
        if let Some(messages) = model["model_messages"].as_object() {
            for key in messages.keys() {
                assert!(
                    [
                        "instructions_template",
                        "instructions_variables",
                        "approvals"
                    ]
                    .contains(&key.as_str()),
                    "{} has unsupported guidance key {key}",
                    model["slug"]
                );
            }
            if let Some(approvals) = messages
                .get("approvals")
                .and_then(serde_json::Value::as_object)
            {
                assert!(approvals.keys().all(|key| key == "on_request"));
            }
        }
    }
    for model in crate::bundled_models_response().unwrap().models {
        assert!(
            model
                .get_model_instructions(None)
                .contains("change files only when the request calls for implementation"),
            "{} must preserve investigation-only intent",
            model.slug
        );
    }
}
