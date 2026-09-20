use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn search_results_defer_functions_and_namespace_children_without_output_schemas() {
    let function = ResponsesApiTool {
        name: "lookup".to_string(),
        description: "Look up a record".to_string(),
        strict: true,
        defer_loading: Some(false),
        parameters: JsonSchema::string(Some("Record ID".to_string())),
        output_schema: Some(serde_json::json!({"type": "string", "description": "Record"})),
    };
    let mut expected_function = function.clone();
    expected_function.defer_loading = Some(true);
    expected_function.output_schema = None;
    let namespace = crate::ResponsesApiNamespace {
        name: "records".to_string(),
        description: "Record tools".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(function.clone())],
    };
    let mut expected_namespace = namespace.clone();
    expected_namespace.tools = vec![ResponsesApiNamespaceTool::Function(
        expected_function.clone(),
    )];

    for (spec, expected) in [
        (
            ToolSpec::Function(function),
            LoadableToolSpec::Function(expected_function),
        ),
        (
            ToolSpec::Namespace(namespace),
            LoadableToolSpec::Namespace(expected_namespace),
        ),
    ] {
        let original = spec.clone();
        let info = ToolSearchInfo::from_tool_spec(&spec, None).expect("searchable tool");
        assert_eq!(info.entry.output, expected);
        assert_eq!(spec, original);
    }
}

#[test]
fn default_search_text_uses_model_visible_namespace_metadata_once() {
    let mut schedule_schema = JsonSchema::object(
        BTreeMap::from([(
            "timezone".to_string(),
            JsonSchema::string(Some("IANA timezone.".to_string())),
        )]),
        /*required*/ None,
        /*additional_properties*/ None,
    );
    schedule_schema.description = Some("Schedule settings.".to_string());
    let mut parameters = JsonSchema::object(
        BTreeMap::from([
            (
                "mode".to_string(),
                JsonSchema::string(Some("Update mode.".to_string())),
            ),
            ("schedule".to_string(), schedule_schema),
        ]),
        /*required*/ None,
        /*additional_properties*/ None,
    );
    parameters.description = Some("Automation options.".to_string());
    let spec = ToolSpec::Namespace(crate::ResponsesApiNamespace {
        name: "codex_app".to_string(),
        description: "Manage Codex automations.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: "automation_update".to_string(),
            description: "Create or update automations.".to_string(),
            strict: false,
            defer_loading: None,
            parameters,
            output_schema: None,
        })],
    });

    let search_info = ToolSearchInfo::from_tool_spec(&spec, /*source_info*/ None)
        .expect("namespace should be searchable");

    assert_eq!(
        search_info.entry.search_text,
        "codex_app Manage Codex automations. codex_app__automation_update automation_update automation update Create or update automations. Automation options. mode Update mode. schedule Schedule settings. timezone IANA timezone."
    );
    assert_eq!(
        search_info.entry.tool_names,
        vec!["automation_update".to_string()]
    );
}

#[test]
fn schema_search_text_indexes_references_compositions_definitions_and_literals() {
    let schema = JsonSchema {
        schema_ref: Some("#/$defs/requestEnvelope".to_string()),
        required: Some(vec!["account_id".to_string()]),
        enum_values: Some(vec![serde_json::json!({"mode": "advanced"})]),
        additional_properties: Some(JsonSchema::string(Some("Extension value".to_string())).into()),
        one_of: Some(vec![JsonSchema::string(Some("One-of branch".to_string()))]),
        all_of: Some(vec![JsonSchema::string(Some("All-of branch".to_string()))]),
        defs: Some(BTreeMap::from([(
            "requestEnvelope".to_string(),
            JsonSchema::string(Some("Definition body".to_string())),
        )])),
        definitions: Some(BTreeMap::from([(
            "legacyPayload".to_string(),
            JsonSchema::string_enum(
                vec![serde_json::json!("legacy-mode")],
                Some("Legacy definition".to_string()),
            ),
        )])),
        ..Default::default()
    };

    let text = schema_search_text(&schema);

    for expected in [
        "#/$defs/requestEnvelope",
        "account_id",
        "mode",
        "advanced",
        "Extension value",
        "One-of branch",
        "All-of branch",
        "requestEnvelope",
        "Definition body",
        "legacyPayload",
        "legacy-mode",
    ] {
        assert!(
            text.contains(expected),
            "missing `{expected}` from `{text}`"
        );
    }
}
