use super::*;
use crate::ModelsManagerConfig;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ApprovalMessages;
use pretty_assertions::assert_eq;

const REQUIRED_PROMPT_RULE_ANCHORS: &[(&str, &str)] = &[
    (
        "nearest sufficient completion",
        "nearest sufficient completion point",
    ),
    ("user-work protection", "first protect user work"),
    (
        "patch success is not validation",
        "Patch success means the patch applied",
    ),
    ("concurrent edit convergence", "Concurrent Edit Convergence"),
    (
        "implementation self-repair",
        "implementation self-repair is mandatory",
    ),
    (
        "scoped nearest-sufficient validation",
        "nearest sufficient tests or checks",
    ),
];

fn assert_prompt_rules(label: &str, prompt: &str) {
    for (rule, anchor) in REQUIRED_PROMPT_RULE_ANCHORS {
        assert!(
            prompt.contains(anchor),
            "{label} should include {rule} rule anchor: {anchor}"
        );
    }
}

#[test]
fn base_instructions_include_prompt_rules_anchors() {
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
