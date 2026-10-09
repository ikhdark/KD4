use super::*;
use crate::ModelsManagerConfig;
use codex_protocol::openai_models::ApprovalMessages;
use pretty_assertions::assert_eq;

#[test]
fn reasoning_summary_support_comes_from_model_metadata() {
    let mut model = model_info_from_slug("unknown-model");
    model.supports_reasoning_summaries = true;
    let config = ModelsManagerConfig::default();

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}

#[test]
fn base_instruction_override_preserves_catalog_approval_messages() {
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
    };
    model.model_messages = Some(ModelMessages {
        token_budget: None,
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

    assert_eq!(updated.base_instructions, "override");
    assert_eq!(updated.get_model_instructions(None), "override");
    assert_eq!(
        updated.model_messages,
        Some(ModelMessages {
            token_budget: None,
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
    };
    model.model_messages = Some(ModelMessages {
        token_budget: None,
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
            token_budget: None,
            instructions_template: None,
            instructions_variables: None,
            approvals: Some(approvals),
        })
    );
}

#[test]
fn model_context_window_override_preserves_defaults_and_respects_maximum() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    for (requested, effective) in [
        (None, 273_000),
        (Some(300_000), 300_000),
        (Some(400_000), 400_000),
        (Some(500_000), 400_000),
    ] {
        let config = ModelsManagerConfig {
            model_context_window: requested,
            ..Default::default()
        };
        let updated = with_config_overrides(model.clone(), &config);
        let mut expected = model.clone();
        expected.context_window = Some(effective);
        assert_eq!(updated, expected, "requested window: {requested:?}");
    }
}

#[test]
fn local_personality_template_contains_the_base_prompt_once() {
    let model = model_info_from_slug("gpt-5.2-codex");

    let rendered =
        model.get_model_instructions(Some(codex_protocol::config_types::Personality::Friendly));

    assert_eq!(rendered.matches(&model.base_instructions).count(), 1);
    assert!(rendered.contains(LOCAL_FRIENDLY_TEMPLATE));
}
