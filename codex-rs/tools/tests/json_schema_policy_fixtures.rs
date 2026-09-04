#![allow(clippy::expect_used)]
use codex_tools::ToolName;
use codex_tools::mcp_tool_to_responses_api_tool;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::sync::Arc;

const OVERSIZED_NOTION_CREATE_PAGE_SCHEMA_PATH: &str =
    "tests/fixtures/json_schema_policy/oversized_notion_create_page_input_schema.json";

#[derive(Debug, Deserialize)]
struct FixtureFile {
    source: String,
    tools: Vec<FixtureTool>,
}

#[derive(Debug, Deserialize)]
struct FixtureTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(default)]
    expected_preserved: Vec<ExpectedValue>,
    #[serde(default)]
    expected_pruned: Vec<String>,
    #[serde(default)]
    expected_dropped_fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedValue {
    pointer: String,
    value: Value,
}

macro_rules! json_schema_policy_fixture_tests {
    ($($test_name:ident => $fixture_path:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                assert_fixture_converts_through_mcp_responses_boundary($fixture_path);
            }
        )+
    };
}

json_schema_policy_fixture_tests! {
    slack_schema_converts_through_mcp_responses_boundary =>
        "tests/fixtures/json_schema_policy/slack.json",
    google_calendar_schema_converts_through_mcp_responses_boundary =>
        "tests/fixtures/json_schema_policy/google_calendar.json",
    google_drive_schema_converts_through_mcp_responses_boundary =>
        "tests/fixtures/json_schema_policy/google_drive.json",
    notion_schema_converts_through_mcp_responses_boundary =>
        "tests/fixtures/json_schema_policy/notion.json",
    microsoft_outlook_email_schema_converts_through_mcp_responses_boundary =>
        "tests/fixtures/json_schema_policy/microsoft_outlook_email.json",
}

#[test]
fn oversized_notion_schema_compacts_through_mcp_responses_boundary() {
    let fixture: FixtureFile = load_fixture(OVERSIZED_NOTION_CREATE_PAGE_SCHEMA_PATH);
    let fixture_tool = fixture
        .tools
        .first()
        .expect("oversized fixture should contain a tool");
    let input_bytes = compact_json_len(&fixture_tool.input_schema);

    let responses_tool = convert_fixture_tool(&fixture, fixture_tool);
    let parameters =
        serde_json::to_value(&responses_tool.parameters).expect("responses parameters serialize");
    let output_bytes = compact_json_len(&parameters);

    assert!(
        output_bytes < input_bytes,
        "compaction should reduce schema size from {input_bytes} bytes"
    );

    assert!(
        parameters.pointer("/description").is_none(),
        "oversized schema should drop its root description"
    );

    let expected_values = [
        (
            "/properties/parent/$ref",
            json!("#/$defs/parent"),
            "retain local parent validation",
        ),
        (
            "/properties/parent/description",
            json!(
                "Request body field. Parent reference controlling whether the page is standalone or a row under a data source."
            ),
            "retain the user-facing parent description",
        ),
        (
            "/properties/children/items",
            json!({"$ref": "#/$defs/block"}),
            "retain nested block validation",
        ),
        (
            "/properties/markdown/type",
            json!("string"),
            "retain top-level argument shape",
        ),
        (
            "/properties/properties/type",
            json!("object"),
            "retain object argument shape",
        ),
    ];
    for (pointer, expected, message) in expected_values {
        assert_eq!(
            parameters.pointer(pointer),
            Some(&expected),
            "oversized schema should {message}"
        );
    }
    assert!(
        parameters.pointer("/$defs").is_some(),
        "oversized schema should retain reachable definitions"
    );
}

fn assert_fixture_converts_through_mcp_responses_boundary(path: &str) {
    let fixture: FixtureFile = load_fixture(path);
    for fixture_tool in &fixture.tools {
        let responses_tool = convert_fixture_tool(&fixture, fixture_tool);
        let parameters = serde_json::to_value(&responses_tool.parameters)
            .expect("responses parameters should serialize");

        let expected_fields = [
            (
                "preserve the tool name",
                json!(fixture_tool.name),
                json!(responses_tool.name),
            ),
            (
                "preserve the tool description",
                json!(fixture_tool.description),
                json!(responses_tool.description),
            ),
            (
                "remain a strict:false tool",
                json!(false),
                json!(responses_tool.strict),
            ),
            (
                "produce object-shaped parameters",
                json!("object"),
                parameters.get("type").cloned().unwrap_or(Value::Null),
            ),
        ];

        for (message, expected, actual) in expected_fields {
            assert_eq!(actual, expected, "{} should {message}", fixture_tool.name);
        }
        assert!(
            parameters.get("properties").is_some_and(Value::is_object),
            "{} should produce a parameters.properties object",
            fixture_tool.name
        );

        for expected in &fixture_tool.expected_preserved {
            assert_eq!(
                parameters.pointer(&expected.pointer),
                Some(&expected.value),
                "{} should preserve {}",
                fixture_tool.name,
                expected.pointer
            );
        }

        for pointer in &fixture_tool.expected_pruned {
            assert!(
                parameters.pointer(pointer).is_none(),
                "{} should prune unreachable definition {pointer}",
                fixture_tool.name
            );
        }

        for pointer in &fixture_tool.expected_dropped_fields {
            assert!(
                fixture_tool.input_schema.pointer(pointer).is_some(),
                "{} fixture should contain expected dropped field {pointer}",
                fixture_tool.name
            );
            assert!(
                parameters.pointer(pointer).is_none(),
                "{} should drop field {pointer} after JsonSchema conversion",
                fixture_tool.name
            );
        }
    }
}

fn load_fixture<T: DeserializeOwned>(path: &str) -> T {
    let path = codex_utils_cargo_bin::find_resource!(path).expect("fixture should resolve");
    let fixture = fs::read_to_string(&path).expect("fixture should be readable");
    serde_json::from_str(&fixture).expect("fixture should contain valid JSON")
}

fn convert_fixture_tool(
    fixture: &FixtureFile,
    fixture_tool: &FixtureTool,
) -> codex_tools::ResponsesApiTool {
    let name = &fixture_tool.name;
    let input_schema = fixture_tool
        .input_schema
        .as_object()
        .expect("tool input_schema should be an object")
        .clone();
    let tool = rmcp::model::Tool::new(
        name.to_string(),
        fixture_tool.description.clone(),
        Arc::new(input_schema),
    );

    mcp_tool_to_responses_api_tool(&ToolName::namespaced(&fixture.source, name), &tool)
        .expect("fixture tool should convert to a responses API tool")
}

fn compact_json_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("value should serialize to compact JSON")
        .len()
}
