use super::*;

fn has_parameter(tool: &ToolSpec, name: &str) -> bool {
    serde_json::to_value(tool)
        .expect("serialize tool")
        .pointer(&format!("/parameters/properties/{name}"))
        .is_some()
}

#[test]
fn environment_id_is_only_exposed_when_requested_for_each_factory() {
    let factories = [
        (
            SEARCH_SOURCE_TOOL_NAME,
            create_search_source_tool as fn(SourceToolOptions) -> ToolSpec,
        ),
        (READ_FILE_SPAN_TOOL_NAME, create_read_file_span_tool),
    ];

    for (tool_name, create_tool) in factories {
        let without_environment_id = create_tool(SourceToolOptions {
            include_environment_id: false,
        });
        let with_environment_id = create_tool(SourceToolOptions {
            include_environment_id: true,
        });

        assert!(
            !has_parameter(&without_environment_id, "environment_id"),
            "{tool_name}"
        );
        assert!(
            has_parameter(&with_environment_id, "environment_id"),
            "{tool_name}"
        );
    }
}

#[test]
fn source_tools_describe_local_environment_selection() {
    let tools = [
        create_search_source_tool(SourceToolOptions {
            include_environment_id: true,
        }),
        create_read_file_span_tool(SourceToolOptions {
            include_environment_id: true,
        }),
    ];

    for tool in tools {
        let tool = serde_json::to_value(tool).expect("serialize tool");
        let description = tool
            .pointer("/description")
            .and_then(serde_json::Value::as_str)
            .expect("tool description");
        let environment_description = tool
            .pointer("/parameters/properties/environment_id/description")
            .and_then(serde_json::Value::as_str)
            .expect("environment_id description");

        assert!(description.contains("local environments only"));
        assert!(environment_description.contains("Select a local environment id"));
        assert!(
            environment_description.contains("omit only when the primary environment is local")
        );
    }
}

#[test]
fn search_source_guides_callers_to_ownership_scoped_paths() {
    let tool = serde_json::to_value(create_search_source_tool(SourceToolOptions {
        include_environment_id: false,
    }))
    .expect("serialize search tool");
    let description = tool
        .pointer("/description")
        .and_then(serde_json::Value::as_str)
        .expect("tool description");
    let paths_description = tool
        .pointer("/parameters/properties/paths/description")
        .and_then(serde_json::Value::as_str)
        .expect("paths description");

    for guidance in ["locate_task once", "owner or closure paths"] {
        assert!(description.contains(guidance), "{guidance}");
        assert!(paths_description.contains(guidance), "{guidance}");
    }
    assert!(description.contains("deliberate repository-wide"));
    assert!(description.contains("capped packet for multiple matches"));
    assert!(description.contains("omissions and ambiguities reported explicitly"));
    assert!(paths_description.contains("Empty remains valid"));
}

#[test]
fn source_tool_count_and_line_parameters_are_integers() {
    let search_tool = serde_json::to_value(create_search_source_tool(SourceToolOptions {
        include_environment_id: false,
    }))
    .expect("serialize search tool");
    let read_tool = serde_json::to_value(create_read_file_span_tool(SourceToolOptions {
        include_environment_id: false,
    }))
    .expect("serialize read tool");

    for parameter in ["max_results", "context_lines"] {
        assert_eq!(
            search_tool
                .pointer(&format!("/parameters/properties/{parameter}/type"))
                .and_then(serde_json::Value::as_str),
            Some("integer"),
            "{parameter}"
        );
    }
    for parameter in ["start_line", "line_count"] {
        assert_eq!(
            read_tool
                .pointer(&format!("/parameters/properties/{parameter}/type"))
                .and_then(serde_json::Value::as_str),
            Some("integer"),
            "{parameter}"
        );
    }
}

#[test]
fn source_tool_numeric_bounds_are_emitted_from_runtime_constants() {
    let cases = [
        (
            create_search_source_tool(SourceToolOptions {
                include_environment_id: false,
            }),
            vec![
                ("max_results", 1_u64, SOURCE_SEARCH_MAX_MATCHES as u64),
                (
                    "context_lines",
                    0_u64,
                    SOURCE_SEARCH_MAX_CONTEXT_LINES as u64,
                ),
            ],
        ),
        (
            create_locate_task_tool(SourceToolOptions {
                include_environment_id: false,
            }),
            vec![
                ("max_files", 1_u64, LOCATE_TASK_MAX_FILES as u64),
                (
                    "max_source_bytes",
                    1_u64,
                    LOCATE_TASK_MAX_SOURCE_BYTES as u64,
                ),
            ],
        ),
        (
            create_read_file_span_tool(SourceToolOptions {
                include_environment_id: false,
            }),
            vec![("line_count", 1_u64, SOURCE_READ_MAX_LINES as u64)],
        ),
    ];

    for (tool, bounds) in cases {
        let tool = serde_json::to_value(tool).expect("serialize source tool");
        for (parameter, minimum, maximum) in bounds {
            let schema = tool
                .pointer(&format!("/parameters/properties/{parameter}"))
                .expect("parameter schema");
            assert_eq!(schema.get("minimum"), Some(&minimum.into()));
            assert_eq!(schema.get("maximum"), Some(&maximum.into()));
        }
    }

    let read_tool = serde_json::to_value(create_read_file_span_tool(SourceToolOptions {
        include_environment_id: false,
    }))
    .expect("serialize read tool");
    let start_line = read_tool
        .pointer("/parameters/properties/start_line")
        .expect("start_line schema");
    assert_eq!(start_line.get("minimum"), Some(&1_u64.into()));
    assert!(start_line.get("maximum").is_none());
}

#[test]
fn source_question_enum_and_closed_object_constraints_are_emitted() {
    let tool = serde_json::to_value(create_search_source_tool(SourceToolOptions {
        include_environment_id: false,
    }))
    .expect("serialize search tool");
    let question = tool
        .pointer("/parameters/properties/source_question")
        .expect("source question schema");
    assert_eq!(
        question.get("required"),
        Some(&serde_json::json!(["kind", "detail"]))
    );
    assert_eq!(question.get("additionalProperties"), Some(&false.into()));
    assert_eq!(
        question.pointer("/properties/kind/enum"),
        Some(&serde_json::json!([
            "unknown_caller",
            "unknown_contract",
            "ambiguous_ownership",
            "incomplete_prior_result",
            "source_changed",
            "validation_dependency"
        ]))
    );
}
