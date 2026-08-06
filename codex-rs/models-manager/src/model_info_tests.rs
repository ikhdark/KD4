use super::*;
use crate::ModelsManagerConfig;
use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ApprovalMessages;
use pretty_assertions::assert_eq;

const REQUIRED_PROMPT_RULE_ANCHORS: &[(&str, &[&str])] = &[
    (
        "nearest sufficient completion",
        &["nearest sufficient completion point"],
    ),
    (
        "user-work protection",
        &[
            "first protect user work",
            "Existing and newly observed changes belong to the user",
        ],
    ),
    (
        "patch success is not validation",
        &[
            "Patch success means the patch applied",
            "Patch success proves only that the patch applied",
        ],
    ),
    (
        "concurrent edit convergence",
        &["Concurrent Edit Convergence", "concurrent changes"],
    ),
    (
        "implementation self-repair",
        &[
            "implementation self-repair is mandatory",
            "Implementation self-repair is required",
        ],
    ),
    (
        "scoped nearest-sufficient validation",
        &[
            "nearest sufficient tests or checks",
            "nearest sufficient validation",
        ],
    ),
];

fn assert_prompt_rules(label: &str, prompt: &str) {
    for (rule, anchors) in REQUIRED_PROMPT_RULE_ANCHORS {
        assert!(
            anchors.iter().any(|anchor| prompt.contains(anchor)),
            "{label} should include {rule} rule anchor: {anchors:?}"
        );
    }
}

#[test]
fn base_instructions_include_prompt_rules_anchors() {
    assert_eq!(BASE_INSTRUCTIONS, BASE_INSTRUCTIONS_DEFAULT);
    assert_prompt_rules("BASE_INSTRUCTIONS", BASE_INSTRUCTIONS);
}

#[test]
fn bundled_catalog_prompts_include_prompt_rules_anchors() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    assert!(
        !response.models.is_empty(),
        "bundled models.json should contain models"
    );

    let mut template_count = 0;
    for model in &response.models {
        assert_prompt_rules(
            &format!("{}.base_instructions", model.slug),
            &model.base_instructions,
        );

        if let Some(model_messages) = &model.model_messages
            && let Some(template) = &model_messages.instructions_template
        {
            template_count += 1;
            assert_prompt_rules(
                &format!("{}.model_messages.instructions_template", model.slug),
                template,
            );
        }
    }

    assert!(
        template_count > 0,
        "bundled models should include template-backed prompts"
    );
}

#[test]
fn protocol_default_base_instructions_include_prompt_rules_anchors() {
    assert_prompt_rules(
        "codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT",
        BASE_INSTRUCTIONS_DEFAULT,
    );
}

#[test]
fn bundled_catalog_omits_behavior_identical_instruction_templates() {
    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    for slug in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.2"] {
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
fn fallback_and_gpt_5_2_prompts_defer_tool_contracts_to_live_specs() {
    const DUPLICATED_TOOL_CONTRACTS: &[&str] = &[
        r#"{"command":["apply_patch""#,
        "## apply_patch",
        "This is a FREEFORM tool",
        "## `update_plan`",
        "(`pending`, `in_progress`, or `completed`)",
        "Do not jump an item from pending to completed",
    ];

    let response = crate::bundled_models_response().expect("bundled models.json should parse");
    let gpt_5_2 = response
        .models
        .iter()
        .find(|model| model.slug == "gpt-5.2")
        .expect("bundled models.json should contain gpt-5.2");

    for (label, prompt) in [
        ("fallback", BASE_INSTRUCTIONS_DEFAULT),
        ("gpt-5.2", gpt_5_2.base_instructions.as_str()),
    ] {
        for duplicated_contract in DUPLICATED_TOOL_CONTRACTS {
            assert!(
                !prompt.contains(duplicated_contract),
                "{label} prompt should defer {duplicated_contract:?} to the live tool spec"
            );
        }
    }
}

#[test]
fn reasoning_summaries_override_true_enables_support() {
    let model = model_info_from_slug("unknown-model");
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(true),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.supports_reasoning_summaries = true;

    assert_eq!(updated, expected);
}

#[test]
fn reasoning_summaries_override_false_does_not_disable_support() {
    let mut model = model_info_from_slug("unknown-model");
    model.supports_reasoning_summaries = true;
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(false),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn reasoning_summaries_override_false_is_noop_when_model_is_false() {
    let model = model_info_from_slug("unknown-model");
    let config = ModelsManagerConfig {
        model_supports_reasoning_summaries: Some(false),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn base_instruction_override_preserves_catalog_approval_messages() {
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
        on_request_auto_review: Some("auto approvals".to_string()),
    };
    model.model_messages = Some(ModelMessages {
        instructions_template: Some("template".to_string()),
        instructions_variables: Some(ModelInstructionsVariables {
            personality_default: Some("default".to_string()),
            personality_friendly: Some("friendly".to_string()),
            personality_pragmatic: Some("pragmatic".to_string()),
        }),
        approvals: Some(approvals.clone()),
    });
    let config = ModelsManagerConfig {
        base_instructions: Some("override".to_string()),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(
        updated.model_messages,
        Some(ModelMessages {
            instructions_template: None,
            instructions_variables: None,
            approvals: Some(approvals),
        })
    );
}

#[test]
fn disabled_personality_preserves_catalog_approval_messages() {
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
        on_request_auto_review: None,
    };
    model.model_messages = Some(ModelMessages {
        instructions_template: Some("template".to_string()),
        instructions_variables: None,
        approvals: Some(approvals.clone()),
    });
    let config = ModelsManagerConfig {
        personality_enabled: false,
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(
        updated.model_messages,
        Some(ModelMessages {
            instructions_template: None,
            instructions_variables: None,
            approvals: Some(approvals),
        })
    );
}

#[test]
fn model_context_window_override_clamps_to_max_context_window() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig {
        model_context_window: Some(500_000),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.context_window = Some(400_000);

    assert_eq!(updated, expected);
}

#[test]
fn model_context_window_uses_model_value_without_override() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig::default();

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}
