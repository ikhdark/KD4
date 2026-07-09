use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const VERIFY_LOCAL_TOOL_NAME: &str = "verify_local";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerifyLocalToolOptions {
    pub(crate) include_environment_id: bool,
}

impl VerifyLocalToolOptions {
    pub(crate) const fn with_verify_local_environment_id(include_environment_id: bool) -> Self {
        Self {
            include_environment_id,
        }
    }
}

pub(crate) fn create_verify_local_tool(options: VerifyLocalToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![json!("plan"), json!("fast"), json!("final")],
                Some("Validation mode to run.".to_string()),
            ),
        ),
        (
            "changed".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Repo-relative file path to validate.".to_string())),
                Some("Explicit changed files to scope validation to.".to_string()),
            ),
        ),
        (
            "staged".to_string(),
            JsonSchema::boolean(Some("Validate currently staged files.".to_string())),
        ),
        (
            "scope_current".to_string(),
            JsonSchema::boolean(Some(
                "Use the verifier's current persisted scope; maps to --scope current.".to_string(),
            )),
        ),
        (
            "no_cache".to_string(),
            JsonSchema::boolean(Some("Bypass verifier cache for this run.".to_string())),
        ),
        (
            "json".to_string(),
            JsonSchema::boolean(Some(
                "Return raw JSON output instead of a compact verdict summary.".to_string(),
            )),
        ),
    ]);
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::any_of(
                vec![
                    JsonSchema::string(Some(
                        "Environment id from <environment_context>.".to_string(),
                    )),
                    JsonSchema::null(None),
                ],
                Some("Target turn environment. Use null for the primary environment.".to_string()),
            ),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: VERIFY_LOCAL_TOOL_NAME.to_string(),
        description:
            "Run bounded repo-local validation through scripts/verify_local.py. This tool only accepts read-only narrowing fields; broad workspace or mutating verifier flags are human CLI-only."
                .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(required_verify_local_fields(options)),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn required_verify_local_fields(options: VerifyLocalToolOptions) -> Vec<String> {
    let mut fields = vec![
        "mode",
        "changed",
        "staged",
        "scope_current",
        "no_cache",
        "json",
    ];
    if options.include_environment_id {
        fields.push("environment_id");
    }

    fields.into_iter().map(ToString::to_string).collect()
}

#[cfg(test)]
#[path = "verify_local_spec_tests.rs"]
mod tests;
