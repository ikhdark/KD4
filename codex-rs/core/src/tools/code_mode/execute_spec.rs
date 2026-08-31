use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

pub(crate) fn create_code_mode_tool(
    code_mode_only: bool,
    has_deferred_tools: bool,
    direct_only_tool_names: &[String],
    eager_nested_tool_descriptions: &[String],
) -> ToolSpec {
    const CODE_MODE_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

    let mut description = codex_code_mode::build_exec_tool_description(
        code_mode_only,
        has_deferred_tools,
        direct_only_tool_names,
    );
    for eager_description in eager_nested_tool_descriptions {
        description.push_str("\n\nEager nested tool contract:\n\n");
        description.push_str(eager_description);
    }

    ToolSpec::Freeform(FreeformTool {
        name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
        description,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn create_code_mode_tool_matches_expected_spec() {
        assert_eq!(
            create_code_mode_tool(
                /*code_mode_only*/ true,
                /*has_deferred_tools*/ false,
                &["direct_helper".to_string()],
                &[],
            ),
            ToolSpec::Freeform(FreeformTool {
                name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
                description: codex_code_mode::build_exec_tool_description(
                    /*code_mode_only*/ true,
                    /*has_deferred_tools*/ false,
                    &["direct_helper".to_string()],
                ),
                format: FreeformToolFormat {
                    r#type: "grammar".to_string(),
                    syntax: "lark".to_string(),
                    definition: r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#
                    .to_string(),
                },
            })
        );
    }

    #[test]
    fn code_mode_defaults_to_the_hard_cap_without_forcing_a_recovery_call() {
        let exec_description = codex_code_mode::build_exec_tool_description(true, false, &[]);

        assert!(exec_description.contains("defaults to the 10000-token hard cap"));
        assert!(
            codex_code_mode::build_wait_tool_description()
                .contains("default to the 10000-token hard cap")
        );
        assert_eq!(
            codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
            codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL
        );
        assert_eq!(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL, 10_000);
    }

    #[test]
    fn eager_nested_tool_contract_is_rendered_in_the_exec_description() {
        let ToolSpec::Freeform(exec) = create_code_mode_tool(
            /*code_mode_only*/ true,
            /*has_deferred_tools*/ false,
            &[],
            &["exec command description\n\ndeclare const tools: { exec_command(args: unknown): Promise<unknown>; };".to_string()],
        ) else {
            panic!("expected code mode exec tool");
        };

        assert!(exec.description.contains("Eager nested tool contract:"));
        assert!(exec.description.contains("exec_command(args:"));
    }
}
