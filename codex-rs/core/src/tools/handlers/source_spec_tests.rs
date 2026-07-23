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
