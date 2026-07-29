use super::*;
use pretty_assertions::assert_eq;

#[test]
fn preset_names_use_mode_display_names() {
    assert_eq!(plan_preset().name, ModeKind::Plan.display_name());
    assert_eq!(default_preset().name, ModeKind::Default.display_name());
    assert_eq!(plan_preset().model, None);
    assert_eq!(
        plan_preset().reasoning_effort,
        Some(Some(ReasoningEffort::Medium))
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

    assert!(!default_instructions.contains("{{KNOWN_MODE_NAMES}}"));

    let known_mode_names = format_mode_names(&TUI_VISIBLE_COLLABORATION_MODES);
    let expected_snippet = format!("Known mode names are: {known_mode_names}.");
    assert!(default_instructions.contains(&expected_snippet));

    assert!(default_instructions.contains(
        "Use the `request_user_input` tool only when it is listed in the available tools"
    ));
    assert!(
        default_instructions.contains("ask the user directly with one concise plain-text question")
    );
    assert!(default_instructions.contains("materially different interpretations"));
    assert!(default_instructions.contains("`performance`"));
    assert!(default_instructions.contains("`optimize`"));
    assert!(default_instructions.contains("`bugs`"));
    assert!(default_instructions.contains("`fix`"));
    assert!(default_instructions.contains("`fix this`"));
    assert!(default_instructions.contains("`do this`"));
    assert!(default_instructions.contains("`double check`"));
    assert!(default_instructions.contains("`make this better`"));
    assert!(default_instructions.contains("`improve`"));
    assert!(default_instructions.contains("`give suggestions`"));
    assert!(default_instructions.contains("`top 10 ways to...`"));
    assert!(default_instructions.contains("`how can we improve this`"));
    assert!(default_instructions.contains("`audit`"));
    assert!(default_instructions.contains("`implement`"));
    assert!(default_instructions.contains("Offer two to four concrete"));
    assert!(default_instructions.contains("selected choices as implementation acceptance"));
}
