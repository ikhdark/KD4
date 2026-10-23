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
        "exact current bytes on removed lines, including indentation",
        "appends at EOF",
        "Add File is create-only and refuses an occupied path",
        "current codex-range source hash for a whole-file replacement",
        "incremental",
        "only remaining changes",
        "FREEFORM",
        "exact range replacement of a region already fully read",
        "whole-file source_sha256, never an excerpt hash",
        "read current source and reconcile before retrying",
        "A patch built in JavaScript can be applied in the same cell as the read that feeds it",
    ] {
        assert!(tool.description.contains(rule), "missing rule: {rule}");
    }
    assert!(!tool.description.contains("Environment ID"));
    assert!(tool.description.contains("A current read_file source hash"));
    assert!(!tool.description.contains("semantic_context"));
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

#[tokio::test]
async fn apply_patch_instruction_example_applies_to_indented_source() {
    let instructions = codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS.replace("\r\n", "\n");
    let example = instructions
        .split_once("```text\n")
        .and_then(|(_, rest)| rest.split_once("\n```"))
        .map(|(example, _)| example.to_string())
        .expect("instructions include a text example");
    // Removed lines keep their exact indentation; an unindented removal of the
    // indented body is rejected, so the example must not teach it.
    let unindented = example.replace("-    print", "-print");
    assert_ne!(unindented, example);
    for (patch, applies) in [(example, true), (unindented, false)] {
        let root = tempfile::tempdir().unwrap();
        let source = "def greet():\n    print(\"Hi\")\n";
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/app.py"), source).unwrap();
        std::fs::write(root.path().join("obsolete.txt"), "obsolete\n").unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(root.path()).unwrap();
        let result = codex_apply_patch::apply_patch(
            &patch,
            &cwd,
            &mut Vec::new(),
            &mut Vec::new(),
            codex_exec_server::LOCAL_FS.as_ref(),
            /*sandbox*/ None,
        )
        .await;
        assert_eq!(result.is_ok(), applies, "{patch}\n{result:?}");
        if applies {
            assert_eq!(
                std::fs::read_to_string(root.path().join("src/main.py")).unwrap(),
                "def greet():\n    print(\"Hello, world!\")\n"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("hello.txt")).unwrap(),
                "Hello world\n"
            );
            assert!(!root.path().join("src/app.py").exists());
            assert!(!root.path().join("obsolete.txt").exists());
        } else {
            // Preflight rejects the whole patch before any operation is written.
            assert_eq!(
                std::fs::read_to_string(root.path().join("src/app.py")).unwrap(),
                source
            );
            assert!(!root.path().join("hello.txt").exists());
            assert!(root.path().join("obsolete.txt").exists());
        }
    }
}
