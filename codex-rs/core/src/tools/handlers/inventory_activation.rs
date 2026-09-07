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

pub struct InventoryActivationHandler;

impl ToolExecutor<ToolInvocation> for InventoryActivationHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("activate_inventory_v2")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "activate_inventory_v2".to_owned(),
            description: "Atomically select the reviewed Inventory V2 bundle for this repository. Requires the root's current private inventory catalog, focused transition or reconciliation approval, and sealed independent historical review. Takes no supplied receipt or approval. Retains unresolved obligations as certification blockers. Does not run tests or activate a Desktop binary.".to_owned(),
            strict: true, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "activate_inventory_v2 requires empty function arguments".to_owned(),
                ));
            };
            let value: serde_json::Value = serde_json::from_str(arguments)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            if value.as_object().is_none_or(|object| !object.is_empty()) {
                return Err(FunctionCallError::RespondToModel(
                    "activate_inventory_v2 accepts no caller-authored scope, approval, or receipt"
                        .to_owned(),
                ));
            }
            let result = invocation
                .session
                .services
                .completion_proof
                .activate_inventory_v2()
                .await;
            let success = result.is_ok();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                result.unwrap_or_else(|e| e),
                Some(success),
            )))
        })
    }
}

impl CoreToolRuntime for InventoryActivationHandler {}
