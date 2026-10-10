use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn affordance_projection_preserves_contract_and_positive_clauses() {
    let spec = ToolSpec::Function(ResponsesApiTool {
        name: "preview".into(),
        description: "Preview drafts; Cannot send email but can list folders. Read labels. Not only read metadata. To search records, use the query parameter. To archive messages, use archive instead.".into(),
        strict: false, defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::from([(
            "includeArchived".into(), JsonSchema::string(Some("Filter records; do not delete messages".into()))
        )]), None, None), output_schema: None,
    });
    let info = ToolSearchInfo::from_tool_spec(&spec, None).unwrap();
    let activation = info.entry.callable_search_text();
    for absent in ["send email", "archive messages", "delete messages"] {
        assert!(!activation.contains(absent), "{activation}");
        assert!(info.entry.search_text.to_lowercase().contains(absent));
    }
    for present in ["Preview drafts", "can list folders", "Read labels", "Not only read metadata", "search records", "include archived", "Filter records"] {
        assert!(activation.contains(present), "{activation}");
    }
    let LoadableToolSpec::Function(output) = info.entry.output.as_ref() else { panic!("function") };
    let ToolSpec::Function(original) = &spec else { panic!("function") };
    assert_eq!(output.description, original.description);
    assert_eq!(output.parameters, original.parameters);
}

#[test]
fn affordance_not_only_keeps_positive_capabilities() {
    for prefix in ["Does not only", "Do not only", "Doesn't only", "DOES NOT   ONLY"] {
        let tool = ResponsesApiTool {
            name: "lookup".into(),
            description: format!("{prefix} read documents but also search records; do not delete files."),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        };
        let info = ToolSearchInfo::from_tool_spec(&ToolSpec::Function(tool.clone()), None).unwrap();
        for activation in [
            info.entry.callable_search_text(),
            ToolSearchEntry::callable_function_search_text(&tool),
        ] {
            assert!(activation.contains("read documents"), "{prefix}: {activation}");
            assert!(activation.contains("search records"), "{prefix}: {activation}");
            assert!(!activation.contains("delete files"), "{prefix}: {activation}");
        }
    }
}

#[test]
fn affordance_output_hints_are_bounded_shared_and_retrieval_only() {
    let schema = serde_json::json!({"type":"object", "properties":{
        "continuationToken":{"type":"string", "description":"Never index output prose"}
    }});
    for output in [schema.clone().into(), crate::ToolOutputSchema::from_mcp_output_schema(
        Some(Arc::new(schema.as_object().unwrap().clone())))] {
        let function = ResponsesApiTool {
            name: "lookup".into(), description: "Read records".into(), strict: false,
            defer_loading: None, parameters: JsonSchema::object(BTreeMap::new(), None, None),
            output_schema: Some(output),
        };
        let shared = ToolSearchInfo::from_shared_spec("Read records".into(),
            Arc::new(LoadableToolSpec::Function(function.clone())), None);
        let owned = ToolSearchInfo::from_tool_spec(&ToolSpec::Function(function), None).unwrap();
        for info in [shared, owned] {
            assert!(info.entry.search_text.contains("continuation token"));
            assert!(!info.entry.callable_search_text().contains("continuation"));
            assert!(!info.entry.search_text.contains("output prose"));
        }
    }
    let properties = (0..1000).map(|i| (format!("field_{i:04}"), serde_json::json!({"type":"string"})))
        .collect::<serde_json::Map<_, _>>();
    let tool = ResponsesApiTool { name: "lookup".into(), description: String::new(), strict: false,
        defer_loading: None, parameters: JsonSchema::object(BTreeMap::new(), None, None),
        output_schema: Some(serde_json::json!({"properties":properties}).into()) };
    let mut text = String::new();
    append_output_search_text(&tool, &mut text);
    assert!(text.len() <= 2048);
    assert!(text.contains("field_0063"));
    assert!(!text.contains("field_0064"));
    // Long names exhaust the byte budget before the field-count limit applies.
    let properties = (0..64).map(|i| (format!("{}_{i:04}", "k".repeat(100)), serde_json::json!({"type":"string"})))
        .collect::<serde_json::Map<_, _>>();
    let tool = ResponsesApiTool { output_schema: Some(serde_json::json!({"properties":properties}).into()), ..tool };
    let mut text = String::new();
    append_output_search_text(&tool, &mut text);
    assert!(text.len() <= 2048);
    assert!(text.contains("_0008"));
    assert!(!text.contains("_0009"));
}

#[test]
fn identifier_words_preserve_acronyms_and_camel_case_boundaries() {
    for (name, expected) in [
        ("_create_calendar_event", "create calendar event"),
        ("createCalendarEvent", "create calendar event"),
        ("getHTTPResponse", "get http response"),
        ("read2DImage", "read2 d image"),
        ("namespace__read-file", "namespace read file"),
    ] {
        assert_eq!(identifier_search_words(name), expected);
    }
}

#[test]
fn search_results_defer_functions_and_namespace_children_without_output_schemas() {
    let function = ResponsesApiTool {
        name: "lookup".to_string(),
        description: "Look up a record".to_string(),
        strict: true,
        defer_loading: Some(false),
        parameters: JsonSchema::string(Some("Record ID".to_string())),
        output_schema: Some(serde_json::json!({"type": "string", "description": "Record"}).into()),
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
        assert_eq!(info.entry.to_loadable_spec(), expected);
        assert_eq!(spec, original);
        let raw = std::sync::Arc::new(match spec {
            ToolSpec::Function(tool) => LoadableToolSpec::Function(tool),
            ToolSpec::Namespace(mut namespace) => {
                namespace.description.clear();
                LoadableToolSpec::Namespace(namespace)
            }
            _ => unreachable!(),
        });
        let shared = ToolSearchInfo::from_shared_spec("lookup".to_string(), raw.clone(), None);
        assert!(std::sync::Arc::ptr_eq(&raw, &shared.entry.output));
        assert!(std::sync::Arc::ptr_eq(&raw, &shared.clone().entry.output));
        let expected = ToolSearchInfo::from_tool_spec(&raw.as_ref().clone().into(), None).unwrap();
        assert_eq!(shared.entry.to_loadable_spec(), expected.entry.to_loadable_spec());
        let raw_function = match raw.as_ref() {
            LoadableToolSpec::Function(tool) => tool,
            LoadableToolSpec::Namespace(namespace) => {
                assert!(namespace.description.is_empty());
                let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0];
                tool
            }
        };
        assert_eq!(raw_function.defer_loading, Some(false));
        assert!(raw_function.output_schema.is_some());
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
    let callable = crate::code_mode_name_for_tool_name(&crate::ToolName::namespaced(
        "codex_app", "automation_update",
    ));

    assert_eq!(
        search_info.entry.search_text,
        format!("codex_app Manage Codex automations. {callable} automation_update automation update Create or update automations. Automation options. mode Update mode. schedule Schedule settings. timezone IANA timezone.")
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
        "mode advanced",
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
