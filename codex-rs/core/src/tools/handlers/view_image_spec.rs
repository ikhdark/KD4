use codex_protocol::models::VIEW_IMAGE_TOOL_NAME;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewImageToolOptions {
    pub can_request_original_image_detail: bool,
    pub include_environment_id: bool,
}

pub fn create_view_image_tool(options: ViewImageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "path".to_string(),
        JsonSchema::string(Some("Local filesystem path to an image file.".to_string())),
    )]);
    let mut detail_levels = vec![json!("low"), json!("high")];
    if options.can_request_original_image_detail {
        detail_levels.push(json!("original"));
    }
    properties.insert(
            "detail".to_string(),
            JsonSchema::string_enum(
                detail_levels,
                Some(
                    "Image detail level. Defaults to `high`; use `low` for lower-cost overview inspection. When available, `original` preserves exact resolution.".to_string(),
                ),
            ),
        );
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>. Omit to use the primary environment."
                    .to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: VIEW_IMAGE_TOOL_NAME.to_string(),
        description: "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["path".to_string()]), Some(false.into())),
        output_schema: Some(view_image_output_schema()),
    })
}

fn view_image_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "image_url": {
                "type": "string",
                "description": "Data URL for the loaded image."
            },
            "detail": {
                "type": "string",
                "enum": ["low", "high", "original"],
                "description": "Image detail hint returned by view_image: `low` for overview inspection, `high` for default resized behavior, or `original` when original resolution is preserved."
            }
        },
        "required": ["image_url", "detail"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_detail_is_advertised_independently_of_original_support() {
        for original in [false, true] {
            let ToolSpec::Function(spec) = create_view_image_tool(ViewImageToolOptions {
                can_request_original_image_detail: original,
                include_environment_id: false,
            }) else {
                panic!("expected function tool");
            };
            let parameters = serde_json::to_value(spec.parameters).unwrap();
            assert_eq!(
                parameters["properties"]["detail"]["enum"],
                if original {
                    json!(["low", "high", "original"])
                } else {
                    json!(["low", "high"])
                }
            );
            assert_eq!(
                spec.output_schema.unwrap()["properties"]["detail"]["enum"],
                json!(["low", "high", "original"])
            );
        }
    }
}
