use super::*;

fn has_parameter(tool: &ToolSpec, name: &str) -> bool {
    serde_json::to_value(tool)
        .expect("serialize tool")
        .pointer(&format!("/parameters/properties/{name}"))
        .is_some()
}

#[test]
fn environment_id_is_only_exposed_when_requested() {
    let single = create_search_source_tool(SourceToolOptions {
        include_environment_id: false,
    });
    let multiple = create_read_file_span_tool(SourceToolOptions {
        include_environment_id: true,
    });

    assert!(!has_parameter(&single, "environment_id"));
    assert!(has_parameter(&multiple, "environment_id"));
}
