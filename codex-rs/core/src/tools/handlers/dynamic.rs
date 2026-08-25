use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolExposure;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::default_namespace_description;
use codex_tools::dynamic_tool_to_responses_api_tool;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;

pub struct DynamicToolHandler {
    tool_name: ToolName,
    spec: ToolSpec,
    exposure: ToolExposure,
}

impl DynamicToolHandler {
    pub fn new(tool: &DynamicToolFunctionSpec) -> Result<Self, serde_json::Error> {
        Self::from_parts(tool, /*namespace*/ None)
    }

    pub fn new_in_namespace(
        namespace: &DynamicToolNamespaceSpec,
        tool: &DynamicToolFunctionSpec,
    ) -> Result<Self, serde_json::Error> {
        Self::from_parts(tool, Some(namespace))
    }

    fn from_parts(
        tool: &DynamicToolFunctionSpec,
        namespace: Option<&DynamicToolNamespaceSpec>,
    ) -> Result<Self, serde_json::Error> {
        let tool_name = ToolName::new(
            namespace.map(|namespace| namespace.name.clone()),
            tool.name.clone(),
        );
        let mut output_tool = dynamic_tool_to_responses_api_tool(tool)?;
        // Exposure controls deferral; tool search restores this marker for deferred results.
        output_tool.defer_loading = None;
        let spec = match namespace {
            Some(namespace) => ToolSpec::Namespace(ResponsesApiNamespace {
                name: namespace.name.clone(),
                description: if namespace.description.trim().is_empty() {
                    default_namespace_description(&namespace.name)
                } else {
                    namespace.description.clone()
                },
                tools: vec![ResponsesApiNamespaceTool::Function(output_tool)],
            }),
            None => ToolSpec::Function(output_tool),
        };
        Ok(Self {
            tool_name,
            spec,
            exposure: if tool.defer_loading {
                ToolExposure::Deferred
            } else {
                ToolExposure::Direct
            },
        })
    }
}

impl ToolExecutor<ToolInvocation> for DynamicToolHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_tool_spec(
            registered_spec.clone(),
            Some(ToolSearchSourceInfo {
                name: "Dynamic tools".to_string(),
                description: Some("Tools provided by the current Codex thread.".to_string()),
            }),
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl DynamicToolHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            call_id,
            payload,
            cancellation_token,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "dynamic tool handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: Value = parse_arguments(&arguments)?;
        let response = request_dynamic_tool(
            &session,
            turn.as_ref(),
            call_id,
            self.tool_name.clone(),
            args,
            cancellation_token,
        )
        .await
        .map_err(FunctionCallError::RespondToModel)?;

        let DynamicToolResponse {
            content_items,
            success,
        } = response;
        let body = content_items
            .into_iter()
            .map(FunctionCallOutputContentItem::from)
            .collect::<Vec<_>>();
        Ok(boxed_tool_output(FunctionToolOutput::from_content(
            body,
            Some(success),
        )))
    }
}

impl CoreToolRuntime for DynamicToolHandler {}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "active turn checks and dynamic tool response registration must remain atomic"
)]
async fn request_dynamic_tool(
    session: &Session,
    turn_context: &TurnContext,
    call_id: String,
    tool_name: ToolName,
    arguments: Value,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<DynamicToolResponse, String> {
    let namespace = tool_name.namespace;
    let tool = tool_name.name;
    let (tx_response, rx_response) = oneshot::channel();
    let event_id = call_id.clone();
    let registered = {
        let mut active = session.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.try_insert_pending_dynamic_tool(call_id.clone(), tx_response)
                    .is_ok()
            }
            None => false,
        }
    };
    if !registered {
        return Err(format!(
            "dynamic tool call id {event_id} is already pending or the turn is no longer active"
        ));
    }

    let started_at = Instant::now();
    session
        .emit_turn_item_started(
            turn_context,
            &TurnItem::DynamicToolCall(DynamicToolCallItem {
                id: call_id.clone(),
                namespace: namespace.clone(),
                tool: tool.clone(),
                arguments: arguments.clone(),
                status: DynamicToolCallStatus::InProgress,
                content_items: None,
                success: None,
                error: None,
                duration: None,
            }),
        )
        .await;
    let response = tokio::select! {
        response = rx_response => response.ok(),
        () = cancellation_token.cancelled() => {
            let mut active = session.active_turn.lock().await;
            if let Some(active_turn) = active.as_mut() {
                active_turn
                    .turn_state
                    .lock()
                    .await
                    .remove_pending_dynamic_tool(&call_id);
            }
            None
        }
    };

    let item = match &response {
        Some(response) => DynamicToolCallItem {
            id: call_id,
            namespace,
            tool,
            arguments,
            status: if response.success {
                DynamicToolCallStatus::Completed
            } else {
                DynamicToolCallStatus::Failed
            },
            content_items: Some(response.content_items.clone()),
            success: Some(response.success),
            error: None,
            duration: Some(started_at.elapsed()),
        },
        None => DynamicToolCallItem {
            id: call_id,
            namespace,
            tool,
            arguments,
            status: DynamicToolCallStatus::Failed,
            content_items: Some(Vec::new()),
            success: Some(false),
            error: Some("dynamic tool call was cancelled before receiving a response".to_string()),
            duration: Some(started_at.elapsed()),
        },
    };
    session
        .emit_turn_item_completed(turn_context, TurnItem::DynamicToolCall(item))
        .await;

    response
        .ok_or_else(|| "dynamic tool call was cancelled before receiving a response".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context_with_rx;
    use crate::state::ActiveTurn;

    #[test]
    fn duplicate_pending_dynamic_tool_ids_are_rejected_without_replacing_the_owner() {
        let mut state = crate::state::TurnState::default();
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();

        assert!(
            state
                .try_insert_pending_dynamic_tool("duplicate".to_string(), first_tx)
                .is_ok()
        );
        assert!(
            state
                .try_insert_pending_dynamic_tool("duplicate".to_string(), second_tx)
                .is_err()
        );

        state
            .remove_pending_dynamic_tool("duplicate")
            .expect("first sender remains registered")
            .send(DynamicToolResponse {
                content_items: Vec::new(),
                success: true,
            })
            .expect("first receiver remains connected");
        assert!(first_rx.blocking_recv().is_ok());
        assert!(second_rx.blocking_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_stops_dynamic_tool_waiting_and_removes_the_pending_call() {
        let (session, turn, events) = make_session_and_context_with_rx().await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let request = tokio::spawn({
            let session = Arc::clone(&session);
            let turn = Arc::clone(&turn);
            let cancellation = cancellation.clone();
            async move {
                request_dynamic_tool(
                    &session,
                    &turn,
                    "cancelled-dynamic-call".to_string(),
                    ToolName::plain("dynamic_test"),
                    serde_json::json!({}),
                    cancellation,
                )
                .await
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("dynamic request emits its start event")
            .expect("event channel remains open");
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), request)
            .await
            .expect("cancellation terminates the dynamic request")
            .expect("request task joins")
            .expect_err("cancelled request fails");
        assert!(error.contains("cancelled"));

        let turn_state = {
            let mut active = session.active_turn.lock().await;
            let active = active.as_mut().expect("active turn remains available");
            Arc::clone(&active.turn_state)
        };
        assert!(
            turn_state
                .lock()
                .await
                .remove_pending_dynamic_tool("cancelled-dynamic-call")
                .is_none(),
            "cancellation must remove the pending sender"
        );
    }

    #[test]
    fn dynamic_handler_preserves_invalid_schema_error() {
        let tool = DynamicToolFunctionSpec {
            name: "invalid_schema".to_string(),
            description: "Invalid schema fixture".to_string(),
            input_schema: serde_json::json!({ "type": "null" }),
            defer_loading: false,
        };

        let err = DynamicToolHandler::new(&tool)
            .err()
            .expect("invalid dynamic schema should be rejected");

        assert!(
            err.to_string()
                .contains("tool input schema must not be a singleton null type"),
            "unexpected conversion error: {err}"
        );
    }
}
