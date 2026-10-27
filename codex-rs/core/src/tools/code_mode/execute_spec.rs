use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

pub(crate) fn create_code_mode_tool(
    code_mode_only: bool,
    direct_delivery_enabled: bool,
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
        direct_delivery_enabled,
        has_deferred_tools,
        direct_only_tool_names,
    );
    if !eager_nested_tool_descriptions.is_empty() {
        description.push_str("\n\nEager nested tool contracts:");
        for eager_description in eager_nested_tool_descriptions {
            description.push_str("\n\n");
            description.push_str(eager_description);
        }
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

