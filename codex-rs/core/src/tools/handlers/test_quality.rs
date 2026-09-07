use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct TestQualityHandler;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewArgs {
    #[serde(default)]
    test_paths: Option<Vec<String>>,
}

impl ToolExecutor<ToolInvocation> for TestQualityHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("review_test_quality")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "review_test_quality".to_owned(),
            description: "Independently assess changed tests against the user's required behavior and observed defect detection. First run the same exact focused tests against a relevant broken implementation and its correction. This tool verifies private execution evidence and reviews assertions and runtime reachability; it never runs tests or changes source. Reuses current quality evidence. Call after adding or changing tests before claiming completion.".to_owned(),
            strict: true,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([("test_paths".to_owned(),
                JsonSchema::any_of(
                    vec![
                        JsonSchema::array(JsonSchema::string(None), None),
                        JsonSchema::null(None),
                    ],
                    Some("Optional exact runtime-derived test paths for one review batch. Every other missing obligation remains blocked.".to_owned()),
                )
            )]), Some(vec!["test_paths".to_owned()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "review_test_quality requires function arguments".to_owned(),
                ));
            };
            let value: ReviewArgs = serde_json::from_str(arguments)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let requested = value
                .test_paths
                .map(|paths| {
                    let unique = paths
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>();
                    if unique.is_empty() || unique.len() != paths.len() {
                        return Err(FunctionCallError::RespondToModel(
                            "quality review paths must be nonempty and unique".to_owned(),
                        ));
                    }
                    Ok(unique)
                })
                .transpose()?;
            let result = invocation
                .session
                .services
                .completion_proof
                .review_test_quality(
                    Arc::clone(&invocation.session),
                    Arc::clone(&invocation.step_context.turn),
                    invocation.cancellation_token,
                    requested,
                )
                .await;
            let success = result.is_ok();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.unwrap_or_else(|e| e),
                Some(success),
            )))
        })
    }
}

impl CoreToolRuntime for TestQualityHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}
