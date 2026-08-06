use super::*;
use codex_collaboration_mode_templates::EXECUTE;
use codex_collaboration_mode_templates::PAIR_PROGRAMMING;
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
fn default_mode_instructions_replace_mode_names_placeholder() {
    let default_instructions = default_preset()
        .developer_instructions
        .expect("default preset should include instructions")
        .expect("default instructions should be set");
    let normalized_instructions = default_instructions
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are: {known_mode_names}.");
    assert!(default_instructions.contains(&expected_snippet));

    assert!(
        default_instructions.contains(
            "Use `request_user_input` only when it is available and a structured choice is"
        )
    );
    assert!(default_instructions.contains("ask one concise plain-text question"));
    assert!(normalized_instructions.contains("ask one targeted question only when all"));
    assert!(default_instructions.contains("cannot reasonably be discovered"));
    assert!(default_instructions.contains("`double check` and `audit` are read-only"));
    assert!(default_instructions.contains("Offer two to four mutually exclusive options"));
    assert!(default_instructions.contains("implementation acceptance"));
    assert!(default_instructions.len() < 3_000);
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
    assert!(EXECUTE.len() < 4_000);
    assert!(PAIR_PROGRAMMING.len() < 5_000);
    assert!(COLLABORATION_MODE_PLAN.len() < 6_000);
}

#[test]
fn auxiliary_mode_templates_keep_their_distinct_execution_contracts() {
    assert!(EXECUTE.contains("Complete the task end to end"));
    assert!(EXECUTE.contains("low-risk, reversible assumption"));
    assert!(EXECUTE.contains("Do not treat user silence as authority"));
    assert!(EXECUTE.contains("Implement the smallest complete change"));

    assert!(PAIR_PROGRAMMING.contains("Treat the user as an active collaborator"));
    assert!(PAIR_PROGRAMMING.contains("meaningful increments"));
    assert!(PAIR_PROGRAMMING.contains("For material choices"));
    assert!(PAIR_PROGRAMMING.contains("structured user-input tool"));
}
