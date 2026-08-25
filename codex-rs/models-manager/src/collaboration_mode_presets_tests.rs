use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preset_names_use_mode_display_names() {
    assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
    assert_eq!(default_preset().name, ModeKind::Default.display_name());
    assert_eq!(plan_preset().model, None);
    assert_eq!(
        plan_preset().reasoning_effort,
        Some(Some(ReasoningEffort::Ultra))
    );
    assert_eq!(default_preset().model, None);
    assert_eq!(default_preset().reasoning_effort, None);
}

#[test]
fn default_mode_instructions_keep_only_the_compact_execution_contract() {
    let default_instructions = default_preset()
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");
    let normalized_instructions = default_instructions
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));
    assert!(!default_instructions.contains("Known mode names"));
    assert!(default_instructions.contains("`request_user_input` is available"));
    assert!(default_instructions.contains("exactly four mutually exclusive suggested answers"));
    assert!(!default_instructions.contains("For example"));
    assert!(default_instructions.contains("requests are read-only"));
    assert!(default_instructions.contains("scoped implementation"));
    assert!(default_instructions.contains("Resolve discoverable facts"));
    assert!(normalized_instructions.contains("one concise direct question"));
    assert!(default_instructions.contains("external-action"));
    assert!(default_instructions.contains("nearest sufficient proof"));
    assert!(default_instructions.len() < 1_000);
}

#[test]
fn plan_mode_instructions_preserve_planning_contract() {
    let plan_instructions = plan_preset()
        .developer_instructions
        .expect("plan preset should include instructions")
        .expect("plan instructions should be set");
    let normalized_instructions = plan_instructions
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    assert!(plan_instructions.contains("Plan Mode remains active"));
    assert!(plan_instructions.contains("`update_plan`"));
    assert!(normalized_instructions.contains("must not be used while Plan Mode is active"));
    assert!(plan_instructions.contains("Do not edit persistent files"));
    assert!(plan_instructions.contains("`request_user_input` is available"));
    assert!(plan_instructions.contains("<proposed_plan>"));
    assert!(plan_instructions.contains("completely replace the prior plan"));
    assert!(plan_instructions.len() < 6_000);
}

#[test]
fn collaboration_mode_templates_stay_within_prompt_budgets() {
    assert!(COLLABORATION_MODE_DEFAULT.len() < 3_000);
    assert!(COLLABORATION_MODE_PLAN.len() < 6_000);
}
