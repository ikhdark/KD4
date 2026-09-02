use std::sync::Arc;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSession;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeSessionProvider;
use codex_code_mode_protocol::CodeModeSessionProviderFuture;
use codex_code_mode_protocol::CodeModeSessionResultFuture;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::DEFAULT_EXEC_YIELD_TIME_MS;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ImageDetail;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::session_runtime as runtime;
use crate::session_runtime::SessionRuntime;

pub struct NoopCodeModeSessionDelegate;

impl CodeModeSessionDelegate for NoopCodeModeSessionDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Err("code mode nested tools are unavailable".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

#[derive(Default)]
pub struct InProcessCodeModeSessionProvider;

impl CodeModeSessionProvider for InProcessCodeModeSessionProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            let session: Arc<dyn CodeModeSession> =
                Arc::new(InProcessCodeModeSession::with_delegate(delegate));
            Ok(session)
        })
    }
}

/// Tries a preferred session provider first and degrades to a compatible local
/// provider when the preferred provider cannot create a session.
pub struct FallbackCodeModeSessionProvider {
    preferred: Arc<dyn CodeModeSessionProvider>,
    fallback: Arc<dyn CodeModeSessionProvider>,
}

impl FallbackCodeModeSessionProvider {
    pub fn new(
        preferred: Arc<dyn CodeModeSessionProvider>,
        fallback: Arc<dyn CodeModeSessionProvider>,
    ) -> Self {
        Self {
            preferred,
            fallback,
        }
    }
}

impl CodeModeSessionProvider for FallbackCodeModeSessionProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            match self.preferred.create_session(Arc::clone(&delegate)).await {
                Ok(session) => Ok(session),
                Err(error) => {
                    tracing::warn!(
                        error,
                        "preferred code-mode session provider unavailable; using fallback"
                    );
                    self.fallback.create_session(delegate).await
                }
            }
        })
    }
}

pub struct InProcessCodeModeSession {
    runtime: SessionRuntime<ProtocolDelegate>,
}

impl InProcessCodeModeSession {
    pub fn new() -> Self {
        Self::with_delegate(Arc::new(NoopCodeModeSessionDelegate))
    }

    pub fn with_delegate(delegate: Arc<dyn CodeModeSessionDelegate>) -> Self {
        Self {
            runtime: SessionRuntime::new(Arc::new(ProtocolDelegate { delegate })),
        }
    }

    pub fn with_delegate_and_task_failure_handler(
        delegate: Arc<dyn CodeModeSessionDelegate>,
        task_failure_handler: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        Self {
            runtime: SessionRuntime::new_with_task_failure_handler(
                Arc::new(ProtocolDelegate { delegate }),
                Some(task_failure_handler),
            ),
        }
    }

    pub async fn execute(&self, request: ExecuteRequest) -> Result<StartedCell, String> {
        let yield_time_ms = request.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_TIME_MS);
        let started = self
            .runtime
            .execute(
                runtime_request(request),
                runtime::ObserveMode::YieldAfter(Duration::from_millis(yield_time_ms)),
            )
            .await
            .map_err(|error| error.to_string())?;
        let cell_id = protocol_cell_id(&started.cell_id);
        let response_cell_id = cell_id.clone();
        let (response_tx, response_rx) = oneshot::channel();
        tokio::spawn(async move {
            let response = started
                .initial_event()
                .await
                .map_err(|error| error.to_string())
                .and_then(|event| runtime_response(&response_cell_id, event));
            let _ = response_tx.send(response);
        });
        Ok(StartedCell::from_result_receiver(cell_id, response_rx))
    }

    pub async fn wait(&self, request: WaitRequest) -> Result<WaitOutcome, String> {
        self.begin_wait(request).await.await
    }

    async fn begin_wait(
        &self,
        request: WaitRequest,
    ) -> CodeModeSessionResultFuture<'static, WaitOutcome> {
        let WaitRequest {
            cell_id,
            yield_time_ms,
        } = request;
        let runtime_cell_id = runtime_cell_id(&cell_id);
        match self
            .runtime
            .begin_observe(&runtime_cell_id, observe_mode_for_yield_time(yield_time_ms))
            .await
        {
            Ok(pending_event) => Box::pin(async move {
                match pending_event.event().await {
                    Ok(event) => Ok(WaitOutcome::LiveCell(runtime_response(&cell_id, event)?)),
                    Err(runtime::Error::MissingCell(_) | runtime::Error::ClosedCell(_)) => {
                        Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)))
                    }
                    Err(error) => Err(error.to_string()),
                }
            }),
            Err(runtime::Error::MissingCell(_) | runtime::Error::ClosedCell(_)) => {
                missing_wait(cell_id)
            }
            Err(error) => Box::pin(async move { Err(error.to_string()) }),
        }
    }

    pub async fn terminate(&self, cell_id: CellId) -> Result<WaitOutcome, String> {
        match self.runtime.terminate(&runtime_cell_id(&cell_id)).await {
            Ok(event) => Ok(WaitOutcome::LiveCell(runtime_response(&cell_id, event)?)),
            Err(runtime::Error::MissingCell(_) | runtime::Error::ClosedCell(_)) => {
                Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)))
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.runtime
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    }
}

fn observe_mode_for_yield_time(yield_time_ms: u64) -> runtime::ObserveMode {
    if yield_time_ms == codex_code_mode_protocol::OWNER_HELD_STATE_CHANGE_YIELD_TIME_MS {
        runtime::ObserveMode::StateChange
    } else {
        runtime::ObserveMode::YieldAfter(Duration::from_millis(yield_time_ms))
    }
}

impl Default for InProcessCodeModeSession {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeModeSession for InProcessCodeModeSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(InProcessCodeModeSession::execute(self, request))
    }

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(InProcessCodeModeSession::wait(self, request))
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(InProcessCodeModeSession::terminate(self, cell_id))
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(InProcessCodeModeSession::shutdown(self))
    }
}

struct ProtocolDelegate {
    delegate: Arc<dyn CodeModeSessionDelegate>,
}

impl runtime::SessionRuntimeDelegate for ProtocolDelegate {
    async fn invoke_tool(
        &self,
        invocation: runtime::NestedToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        self.delegate
            .invoke_tool(
                CodeModeNestedToolCall {
                    cell_id: protocol_cell_id(&invocation.cell_id),
                    parent_tool_call_id: Some(invocation.parent_tool_call_id),
                    runtime_tool_call_id: invocation.runtime_tool_call_id,
                    tool_name: codex_protocol::ToolName {
                        name: invocation.tool_name.name,
                        namespace: invocation.tool_name.namespace,
                    },
                    tool_kind: match invocation.tool_kind {
                        runtime::ToolKind::Function => CodeModeToolKind::Function,
                        runtime::ToolKind::Freeform => CodeModeToolKind::Freeform,
                    },
                    input: invocation.input,
                },
                cancellation_token,
            )
            .await
    }

    async fn notify(
        &self,
        call_id: String,
        cell_id: runtime::CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> Result<(), String> {
        self.delegate
            .notify(
                call_id,
                protocol_cell_id(&cell_id),
                text,
                cancellation_token,
            )
            .await
    }

    fn cell_closed(&self, cell_id: &runtime::CellId) {
        self.delegate.cell_closed(&protocol_cell_id(cell_id));
    }
}

fn runtime_request(request: ExecuteRequest) -> runtime::CreateCellRequest {
    const DEFAULT_TOOL_TIMEOUT_MS: u64 = 60_000;
    runtime::CreateCellRequest {
        tool_call_id: request.tool_call_id,
        enabled_tools: request
            .enabled_tools
            .into_iter()
            .map(|definition| runtime::ToolDefinition {
                name: definition.name,
                tool_name: runtime::ToolName {
                    name: definition.tool_name.name,
                    namespace: definition.tool_name.namespace,
                },
                description: definition.description,
                kind: match definition.kind {
                    CodeModeToolKind::Function => runtime::ToolKind::Function,
                    CodeModeToolKind::Freeform => runtime::ToolKind::Freeform,
                },
            })
            .collect(),
        source: request.source,
        default_tool_timeout_ms: DEFAULT_TOOL_TIMEOUT_MS,
    }
}

fn runtime_cell_id(cell_id: &CellId) -> runtime::CellId {
    runtime::CellId::new(cell_id.as_str())
}

fn protocol_cell_id(cell_id: &runtime::CellId) -> CellId {
    CellId::new(cell_id.as_str().to_string())
}

fn runtime_response(
    cell_id: &CellId,
    event: runtime::CellEvent,
) -> Result<RuntimeResponse, String> {
    match event {
        runtime::CellEvent::Yielded { content_items } => Ok(RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: content_items.into_iter().map(output_item).collect(),
        }),
        runtime::CellEvent::ExplicitYield { content_items } => Ok(RuntimeResponse::ExplicitYield {
            cell_id: cell_id.clone(),
            content_items: content_items.into_iter().map(output_item).collect(),
        }),
        runtime::CellEvent::Completed {
            content_items,
            error_text,
        } => Ok(RuntimeResponse::Result {
            cell_id: cell_id.clone(),
            content_items: content_items.into_iter().map(output_item).collect(),
            error_text,
        }),
        runtime::CellEvent::Terminated { content_items } => Ok(RuntimeResponse::Terminated {
            cell_id: cell_id.clone(),
            content_items: content_items.into_iter().map(output_item).collect(),
        }),
    }
}

fn output_item(item: runtime::OutputItem) -> FunctionCallOutputContentItem {
    match item {
        runtime::OutputItem::Text { text } => FunctionCallOutputContentItem::InputText { text },
        runtime::OutputItem::Image { image_url, detail } => {
            FunctionCallOutputContentItem::InputImage {
                image_url,
                detail: detail.map(|detail| match detail {
                    runtime::ImageDetail::Auto => ImageDetail::Auto,
                    runtime::ImageDetail::Low => ImageDetail::Low,
                    runtime::ImageDetail::High => ImageDetail::High,
                    runtime::ImageDetail::Original => ImageDetail::Original,
                }),
            }
        }
    }
}

fn missing_cell_response(cell_id: CellId) -> RuntimeResponse {
    RuntimeResponse::Result {
        error_text: Some(format!("exec cell {cell_id} not found")),
        cell_id,
        content_items: Vec::new(),
    }
}

fn missing_wait(cell_id: CellId) -> CodeModeSessionResultFuture<'static, WaitOutcome> {
    Box::pin(async move { Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id))) })
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "service_contract_tests.rs"]
mod contract_tests;
