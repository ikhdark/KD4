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
        let source = match registered_spec {
            ToolSpec::Namespace(namespace) => ToolSearchSourceInfo {
                name: namespace.name.clone(),
                description: Some(namespace.description.clone()),
            },
            _ => ToolSearchSourceInfo {
                name: "Dynamic tools".to_string(),
                description: Some("Tools provided by the current Codex thread.".to_string()),
            },
        };
        ToolSearchInfo::from_tool_spec(registered_spec, Some(source))
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
            &turn,
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
        let mut body = content_items
            .into_iter()
            .map(FunctionCallOutputContentItem::from)
            .collect::<Vec<_>>();
        // The success flag is internal metadata, not part of the provider wire
        // payload or the default code-mode result. Preserve a reported failure
        // in both consumers without replacing the client's multimodal evidence.
        if !success {
            body.insert(
                0,
                FunctionCallOutputContentItem::InputText {
                    text: "Dynamic tool response has success=false.".to_string(),
                },
            );
        }
        Ok(boxed_tool_output(FunctionToolOutput::from_content(
            body,
            Some(success),
        )))
    }
}

impl CoreToolRuntime for DynamicToolHandler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "active turn checks and dynamic tool response registration must remain atomic"
)]
async fn request_dynamic_tool(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    call_id: String,
    tool_name: ToolName,
    arguments: Value,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<DynamicToolResponse, String> {
    let namespace = tool_name.namespace;
    let tool = tool_name.name;
    let cleanup = tokio_util::sync::CancellationToken::new();
    let _cleanup_on_drop = cleanup.clone().drop_guard();
    let (tx_response, mut rx_response) = oneshot::channel();
    let event_id = call_id.clone();
    let originating_turn_state = {
        let mut active = session.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                if crate::session::active_turn_has_other_id(at, &turn_context.sub_id) {
                    return Err("dynamic tool originating turn is no longer active".to_string());
                }
                let mut ts = at.turn_state.lock().await;
                ts.try_insert_pending_dynamic_tool(
                    call_id.clone(),
                    turn_context.sub_id.clone(),
                    tx_response,
                )
                .ok()
                .map(|()| Arc::clone(&at.turn_state))
            }
            None => None,
        }
    };
    let Some(originating_turn_state) = originating_turn_state else {
        return Err(format!(
            "dynamic tool call id {event_id} is already pending or the turn is no longer active"
        ));
    };
    let cleanup_turn_state = Arc::clone(&originating_turn_state);
    let cleanup_key = call_id.clone();
    session.terminal_tasks.spawn(async move {
        cleanup.cancelled().await;
        cleanup_turn_state
            .lock()
            .await
            .remove_closed_pending_dynamic_tool(&cleanup_key);
    });

    let started_at = Instant::now();
    let start_item = TurnItem::DynamicToolCall(DynamicToolCallItem {
        id: call_id.clone(),
        namespace: namespace.clone(),
        tool: tool.clone(),
        arguments: arguments.clone(),
        status: DynamicToolCallStatus::InProgress,
        content_items: None,
        success: None,
        error: None,
        duration: None,
    });
    let start_event = session.turn_item_started_event(turn_context, start_item);
    let append_session = Arc::clone(session);
    let append_event = start_event.clone();
    let append_cancellation = cancellation_token.clone();
    let mut append = Box::pin(async move {
        if append_cancellation.is_cancelled() {
            return false;
        }
        append_session.record_dynamic_tool_event(append_event).await
    });
    // Cancellation can stop observing the ordered append, but cannot revoke
    // admission. Keep the unfinished future for the terminal owner below.
    let recorded = tokio::select! {
        biased;
        () = cancellation_token.cancelled() => None,
        recorded = &mut append => Some(recorded),
    };
    let mut canonical = None;
    let mut legacy = None;
    let dispatch_finished = if recorded.is_some() {
        tokio::select! {
            biased;
            () = cancellation_token.cancelled() => false,
            () = session.dispatch_dynamic_tool_start(
                turn_context, start_event, &mut canonical, &mut legacy,
            ) => true,
        }
    } else {
        false
    };
    let canonical_accepted = canonical.is_some();
    let legacy_accepted = legacy.is_some();
    let dispatched = canonical_accepted || legacy_accepted;
    // A response to an accepted canonical Begin can already be ready while
    // the compatibility request is backpressured. Keep its existing priority.
    let response = if dispatched {
        tokio::select! {
            biased;
            response = &mut rx_response => response.ok(),
            () = cancellation_token.cancelled() => None,
        }
    } else {
        None
    };

    let missing_response = match (canonical_accepted, legacy_accepted, dispatch_finished) {
        (true, true, _) => {
            "dynamic tool call was cancelled or disconnected after dispatch; completion unknown; inspect before retrying"
        }
        (true, false, _) => {
            "dynamic tool canonical start was accepted but the legacy request was not; completion unknown; inspect before retrying"
        }
        (false, true, _) => {
            "dynamic tool legacy request was accepted but the canonical start was not; completion unknown; inspect before retrying"
        }
        (false, false, true) => "dynamic tool start was rejected by the event channel; not dispatched",
        (false, false, false) => "dynamic tool call was cancelled before dispatch; not dispatched",
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
            error: Some(missing_response.to_string()),
            duration: Some(started_at.elapsed()),
        },
    };
    // The pending owner is retired before event backpressure can delay return.
    rx_response.close();
    originating_turn_state
        .lock()
        .await
        .remove_closed_pending_dynamic_tool(&event_id);
    if !dispatched && recorded == Some(false) {
        return Err(missing_response.to_string());
    }
    let terminal_session = Arc::clone(session);
    let terminal_turn = Arc::clone(turn_context);
    let mut terminal = session.terminal_tasks.spawn(async move {
        let item = TurnItem::DynamicToolCall(item);
        if dispatched {
            terminal_session
                .emit_turn_item_completed_with_legacy_events(&terminal_turn, item, legacy_accepted)
                .await;
        } else if recorded == Some(true) || (recorded.is_none() && append.await) {
            // If cancellation won during ordered append, settle that same
            // future here. It may already own a writer admission. Do not
            // dispatch a new live start after the response owner has retired.
            terminal_session
                .record_dynamic_tool_completion(&terminal_turn, item)
                .await;
        }
    });
    // Bound only the observation. Dropping the join handle detaches the one
    // tracked publisher without interrupting persistence or either event path.
    tokio::select! {
        result = tokio::time::timeout(std::time::Duration::from_secs(30), &mut terminal) => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(call_id = %event_id, %error, "dynamic tool completion publisher failed");
                }
                Err(_) => {
                    tracing::warn!(call_id = %event_id, "dynamic tool completion observation timed out; publication remains owned");
                }
            }
        }
        _ = cancellation_token.cancelled() => {},
    }

    response.ok_or_else(|| missing_response.to_string())
}

