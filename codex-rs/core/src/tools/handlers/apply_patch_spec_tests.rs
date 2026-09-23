use super::*;
use pretty_assertions::assert_eq;

#[test]
fn create_apply_patch_freeform_tool_matches_expected_spec() {
    let ToolSpec::Freeform(tool) = create_apply_patch_freeform_tool(false) else {
        panic!("expected freeform tool");
    };
    assert_eq!(tool.name, "apply_patch");
    assert!(
        tool.description
            .contains(codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS)
    );
    assert!(
        tool.description
            .contains("Include only enough context to identify the change uniquely")
    );
    assert!(
        tool.description
            .contains("Do not include unified-diff headers")
    );
    assert_eq!(
        tool.format.definition,
        format!("start: begin_patch (hunk+ | retry) end_patch\n{APPLY_PATCH_LARK_GRAMMAR}")
    );
    for rule in [
        "one or many files",
        "only once per patch",
        "top to bottom",
        "exact indentation",
        "appends at EOF",
        "replaces an existing file",
        "incremental",
        "only remaining changes",
        "FREEFORM",
    ] {
        assert!(tool.description.contains(rule), "missing rule: {rule}");
    }
    assert!(!tool.description.contains("Environment ID"));
    assert_eq!(tool.format.r#type, "grammar");
    assert_eq!(tool.format.syntax, "lark");
    assert_eq!(
        tool.description
            .matches(codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS)
            .count(),
        1
    );
}

#[test]
fn create_apply_patch_freeform_tool_includes_environment_id_when_requested() {
    let ToolSpec::Freeform(tool) =
        create_apply_patch_freeform_tool(/*include_environment_id*/ true)
    else {
        panic!("expected freeform tool");
    };

    assert!(tool.description.contains("*** Environment ID: <id>"));
    assert!(
        tool.description
            .contains("immediately after `*** Begin Patch`")
    );
    assert!(tool.format.definition.contains("environment_id?"));
    assert_eq!(tool.format.definition.matches("start:").count(), 1);
    assert!(tool.format.definition.ends_with(APPLY_PATCH_LARK_GRAMMAR));
    assert!(
        tool.format
            .definition
            .contains("\"*** Environment ID: \" filename LF")
    );
}
