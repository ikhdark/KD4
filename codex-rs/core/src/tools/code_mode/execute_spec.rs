use codex_code_mode::ToolDefinition as CodeModeToolDefinition;
use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) fn create_code_mode_tool(
    enabled_tools: &[CodeModeToolDefinition],
    deferred_tools: &[CodeModeToolDefinition],
    namespace_descriptions: &BTreeMap<String, codex_code_mode::ToolNamespaceDescription>,
    code_mode_only: bool,
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
        description: codex_code_mode::build_exec_tool_description_with_direct_only_tools(
            enabled_tools,
            deferred_tools,
            namespace_descriptions,
            code_mode_only,
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
    use codex_tools::ToolName;
    use codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS;
    use pretty_assertions::assert_eq;

    #[test]
    fn create_code_mode_tool_matches_expected_spec() {
        let enabled_tools = vec![codex_code_mode::ToolDefinition {
            name: "update_plan".to_string(),
            tool_name: ToolName::plain("update_plan"),
            description: "Update the plan".to_string(),
            kind: codex_code_mode::CodeModeToolKind::Function,
            input_schema: None,
            output_schema: None,
        }];

        assert_eq!(
            create_code_mode_tool(
                &enabled_tools,
                &[],
                &BTreeMap::new(),
                /*code_mode_only*/ true,
                &["direct_helper".to_string()],
            ),
            ToolSpec::Freeform(FreeformTool {
                name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
                description: codex_code_mode::build_exec_tool_description_with_direct_only_tools(
                    &enabled_tools,
                    &[],
                    &BTreeMap::new(),
                    /*code_mode_only*/ true,
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
    fn optimization_priority_code_mode_builds_coherent_packets_at_the_shared_limit() {
        let exec_description =
            codex_code_mode::build_exec_tool_description(&[], &[], &BTreeMap::new(), true);

        assert!(exec_description.contains("coherent nested evidence packet"));
        assert!(exec_description.contains("defaults to 10000"));
        assert!(
            codex_code_mode::build_wait_tool_description()
                .contains("coherent packet of up to 10000 tokens")
        );
        assert_eq!(
            codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL,
            DEFAULT_SUCCESS_OUTPUT_TOKENS
        );
    }
}
