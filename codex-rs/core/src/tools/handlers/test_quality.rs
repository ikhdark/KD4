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
            parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "review_test_quality requires empty function arguments".to_owned(),
                ));
            };
            let value: serde_json::Value = serde_json::from_str(arguments)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            if value.as_object().is_none_or(|object| !object.is_empty()) {
                return Err(FunctionCallError::RespondToModel(
                    "review_test_quality takes no author-provided scope, approval, or receipt"
                        .to_owned(),
                ));
            }
            let result = invocation
                .session
                .services
                .completion_proof
                .review_test_quality(
                    Arc::clone(&invocation.session),
                    Arc::clone(&invocation.step_context.turn),
                    invocation.cancellation_token,
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
