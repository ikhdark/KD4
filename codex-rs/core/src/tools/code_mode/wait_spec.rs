use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::adaptive_output_budget_description;
use std::collections::BTreeMap;

pub(crate) fn create_wait_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "cell_id".to_string(),
            JsonSchema::string(Some("Identifier of the running exec cell.".to_string())),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                "Internal observation/progress cadence for an unchanged cell. Empty observations do not cause a model-visible yield or a new model generation, and this does not set a completion deadline. Defaults to 10000 ms."
                    .to_string(),
            )),
        ),
        (
            "max_tokens".to_string(),
            JsonSchema::number(Some(format!(
                "Output token budget for this wait call. {}.",
                adaptive_output_budget_description()
            ))),
        ),
        (
            "terminate".to_string(),
            JsonSchema::boolean(Some(
                "True stops the running exec cell; false or omitted waits for output.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: codex_code_mode::WAIT_TOOL_NAME.to_string(),
        description: format!(
            "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
            codex_code_mode::PUBLIC_TOOL_NAME,
            codex_code_mode::build_wait_tool_description().trim()
        ),
        strict: false,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["cell_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
        defer_loading: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
    use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
    use pretty_assertions::assert_eq;

    #[test]
    fn create_wait_tool_matches_expected_spec() {
        assert_eq!(
            create_wait_tool(),
            ToolSpec::Function(ResponsesApiTool {
                name: codex_code_mode::WAIT_TOOL_NAME.to_string(),
                description: format!(
                    "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
                    codex_code_mode::PUBLIC_TOOL_NAME,
                    codex_code_mode::build_wait_tool_description().trim()
                ),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([
                        (
                            "cell_id".to_string(),
                            JsonSchema::string(Some(
                                "Identifier of the running exec cell.".to_string()
                            )),
                        ),
                        (
                            "max_tokens".to_string(),
                            JsonSchema::number(Some(
                                "Output token budget for this wait call. Defaults adaptively to 4000 tokens for success, 8000 for failure/timeout, and up to 10000 for high-signal diagnostics."
                                    .to_string(),
                            )),
                        ),
                        (
                            "terminate".to_string(),
                            JsonSchema::boolean(Some(
                                "True stops the running exec cell; false or omitted waits for output."
                                    .to_string(),
                            )),
                        ),
                        (
                            "yield_time_ms".to_string(),
                            JsonSchema::number(Some(
                                "Internal observation/progress cadence for an unchanged cell. Empty observations do not cause a model-visible yield or a new model generation, and this does not set a completion deadline. Defaults to 10000 ms."
                                    .to_string(),
                            )),
                        ),
                    ]),
                    Some(vec!["cell_id".to_string()]),
                    Some(false.into()),
                ),
                output_schema: None,
            })
        );
    }

    #[test]
    fn owned_wait_parameter_descriptions_and_schema_shapes_match_runtime_semantics() {
        let ToolSpec::Function(code_wait) = create_wait_tool() else {
            panic!("code-mode wait must remain a function tool");
        };
        let code_properties = code_wait
            .parameters
            .properties
            .as_ref()
            .expect("code-mode wait properties");
        assert_eq!(
            code_properties.keys().cloned().collect::<Vec<_>>(),
            vec!["cell_id", "max_tokens", "terminate", "yield_time_ms"]
        );
        assert!(
            code_properties["yield-time_ms"]
                .description
                .as_deref()
                .is_some_and(
                    |description| description.contains("does not set a completion deadline")
                )
        );

        let ToolSpec::Function(agent_wait) = create_wait_agent_tool_v2(WaitAgentTimeoutOptions {
            default_timeout_ms: 30_000,
            min_timeout_ms: 10_000,
            max_timeout_ms: 3_600_000,
        }) else {
            panic!("multi-agent wait must remain a function tool");
        };
        let agent_properties = agent_wait
            .parameters
            .properties
            .as_ref()
            .expect("multi-agent wait properties");
        assert_eq!(
            agent_properties.keys().cloned().collect::<Vec<_>>(),
            vec!["cursor", "timeout_ms"]
        );
        assert!(
            agent_properties["timeout_ms"]
                .description
                .as_deref()
                .is_some_and(|description| {
                    description.contains("Explicit caller deadline")
                        && description.contains("internal maintenance cadence only")
                })
        );
    }
}
