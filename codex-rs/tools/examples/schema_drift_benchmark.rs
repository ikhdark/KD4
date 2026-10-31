//! Bounded component observations for KDR's discovery --workload schema-drift.
//! Exercises production parsing, effective-contract normalization and declaration
//! rendering. Timings are component costs, not end-to-end performance claims.
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

fn tool(schema: &Value) -> ToolSpec {
    let raw: rmcp::model::Tool = serde_json::from_value(json!({
        "name": "lookup", "description": "Read a record.", "inputSchema": schema,
    }))
    .expect("bounded MCP metadata");
    ToolSpec::Function(codex_tools::tool_definition_to_responses_api_tool(
        codex_tools::parse_mcp_tool(&raw).expect("MCP contract"),
    ))
}

fn baseline(schema: &Value) -> ToolSpec {
    let mut spec = tool(schema);
    // Source-bound control for the previous original-schema disclosure owner:
    // MCP copied producer serialization directly into this description.
    if let ToolSpec::Function(tool) = &mut spec {
        tool.description = format!(
            "Read a record.\nThe parameter schema is a permissive projection. The following original JSON Schema is authoritative and is validated before dispatch:\n{schema}",
        );
    }
    spec
}

fn effective(spec: &ToolSpec, schema: &Value) -> String {
    spec.effective_callable_contract(&ToolName::plain("lookup"), Some(schema))
        .expect("exact callable")
        .to_string()
}

fn main() {
    let schema = json!({
        "type": "object",
        "properties": {
            "value": {"type": ["string", "null"]},
            "tag": {"type": "string"},
            "other": {"type": "string"},
        },
        "required": ["value", "tag"],
        "dependentRequired": {"value": ["other"]},
    });
    // Reorder object serialization and known set-valued keywords only.
    let mut noise = schema.clone();
    noise["required"] = json!(["tag", "value"]);
    noise["properties"]["value"]["type"] = json!(["null", "string"]);
    let object = noise
        .as_object()
        .expect("object")
        .iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let noise = Value::Object(object);
    let before = baseline(&schema);
    let before_noise = baseline(&noise);
    let after = tool(&schema);
    let after_noise = tool(&noise);
    let mut visibility = after.clone();
    if let ToolSpec::Function(tool) = &mut visibility {
        tool.defer_loading = Some(true);
    }

    let mut constraints = schema.clone();
    constraints["dependentRequired"] = json!({"tag": ["other"]});
    let mut literal = schema.clone();
    literal["properties"]["value"]["default"] = json!(["a", "b"]);
    let mut literal_reordered = literal.clone();
    literal_reordered["properties"]["value"]["default"] = json!(["b", "a"]);
    let mut output = after.clone();
    if let ToolSpec::Function(tool) = &mut output {
        tool.output_schema = Some(json!({"type": "string"}).into());
    }
    let mut guidance = after.clone();
    if let ToolSpec::Function(tool) = &mut guidance {
        tool.description
            .push_str(" Only use explicit record identifiers.");
    }
    let freeform = |definition: &str| {
        ToolSpec::Freeform(codex_tools::FreeformTool {
            name: "lookup".into(),
            description: "Raw record lookup".into(),
            format: codex_tools::FreeformToolFormat {
                r#type: "grammar".into(),
                syntax: "lark".into(),
                definition: definition.into(),
            },
        })
    };

    let ToolSpec::Function(member) = after.clone() else {
        unreachable!()
    };
    let mut sibling = member.clone();
    sibling.name = "sibling".into();
    let namespace = ToolSpec::Namespace(ResponsesApiNamespace {
        name: "records".into(),
        description: "Record operations".into(),
        tools: vec![
            ResponsesApiNamespaceTool::Function(member),
            ResponsesApiNamespaceTool::Function(sibling),
        ],
    });
    let mut changed_namespace = namespace.clone();
    if let ToolSpec::Namespace(namespace) = &mut changed_namespace {
        let ResponsesApiNamespaceTool::Function(sibling) = &mut namespace.tools[1];
        sibling.description.push_str(" Changed sibling.");
        namespace.tools.reverse();
    }
    let identity = ToolName::namespaced("records", "lookup");
    let sibling_isolated = namespace.effective_callable_contract(&identity, Some(&schema))
        == changed_namespace.effective_callable_contract(&identity, Some(&schema));
    let identity_isolated = namespace
        .effective_callable_contract(&ToolName::namespaced("wrong", "lookup"), Some(&schema))
        .is_none();
    let definitions = |schema: &Value| {
        codex_tools::collect_code_mode_tool_definitions_with_input_schemas(
            [&after],
            &HashMap::from([(ToolName::plain("lookup"), schema.clone())]),
        )
    };
    let mut changed_type = schema.clone();
    changed_type["properties"]["value"]["type"] = json!("integer");
    let declaration_refreshed =
        definitions(&schema)[0].description != definitions(&changed_type)[0].description;
    let meaningful_changes_detected = [
        effective(&after, &schema) != effective(&tool(&constraints), &constraints),
        effective(&tool(&literal), &literal)
            != effective(&tool(&literal_reordered), &literal_reordered),
        effective(&after, &schema) != effective(&output, &schema),
        effective(&after, &schema) != effective(&guidance, &schema),
        effective(&freeform("start: /a/"), &Value::Null)
            != effective(&freeform("start: /b/"), &Value::Null),
    ]
    .into_iter()
    .filter(|changed| *changed)
    .count();

    let rows = (0..3)
        .map(|repetition| {
            let mut baseline_false_invalidations = 0;
            let started = Instant::now();
            for _ in 0..200 {
                baseline_false_invalidations += usize::from(
                    black_box(before.callable_contract().to_string())
                        != black_box(before_noise.callable_contract().to_string()),
                );
                baseline_false_invalidations += usize::from(
                    black_box(after.callable_contract().to_string())
                        != black_box(visibility.callable_contract().to_string()),
                );
            }
            let baseline_ns = started.elapsed().as_nanos();
            let mut candidate_false_invalidations = 0;
            let started = Instant::now();
            for _ in 0..200 {
                candidate_false_invalidations += usize::from(
                    black_box(effective(&after, &schema))
                        != black_box(effective(&after_noise, &noise)),
                );
                candidate_false_invalidations += usize::from(
                    black_box(effective(&after, &schema))
                        != black_box(effective(&visibility, &schema)),
                );
            }
            json!({"repetition": repetition, "iterations": 200,
            "baseline_false_invalidations": baseline_false_invalidations,
            "candidate_false_invalidations": candidate_false_invalidations,
            "baseline_ns": baseline_ns, "candidate_ns": started.elapsed().as_nanos(),
            "meaningful_changes_detected": meaningful_changes_detected,
            "sibling_isolated": sibling_isolated, "identity_isolated": identity_isolated,
            "declaration_refreshed": declaration_refreshed})
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        json!({"workload": "effective-tool-contracts-v1", "rows": rows})
    );
}
