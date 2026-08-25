use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

pub(crate) fn create_code_mode_tool(
    code_mode_only: bool,
    has_deferred_tools: bool,
    direct_only_tool_names: &[String],
) -> ToolSpec {
    const CODE_MODE_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

    ToolSpec::Freeform(FreeformTool {
        name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
        description: codex_code_mode::build_exec_tool_description(
            code_mode_only,
            has_deferred_tools,
            direct_only_tool_names,
        ),
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
    fn optimization_priority_code_mode_builds_coherent_packets_at_the_code_mode_limit() {
        let exec_description = codex_code_mode::build_exec_tool_description(true, false, &[]);

        assert!(exec_description.contains("defaults to 10000"));
        assert!(
            codex_code_mode::build_wait_tool_description()
                .contains("coherent packet of up to 10000 tokens")
        );
        assert_eq!(
            codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
            10_000
        );
    }
}
