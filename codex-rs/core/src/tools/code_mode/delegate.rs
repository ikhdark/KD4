use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NestedCancellation;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::call_nested_tool;
use crate::session::step_context::StepContext;
use crate::session::turn_execution::PendingOwnerDrainedContinuation;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

const MAX_PENDING_CONTINUATIONS_PER_CELL: usize = 64;
const MAX_QUEUED_DISPATCHES: usize = 128;

struct CellDispatchState {
    ready: watch::Sender<bool>,
    terminal: bool,
    pending_continuations: Vec<PendingOwnerDrainedContinuation>,
}

type CellDispatchStates = Arc<Mutex<HashMap<CellId, CellDispatchState>>>;

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    cells: CellDispatchStates,
    dequeue_owner: Arc<Mutex<Option<Arc<()>>>>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new() -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::bounded(MAX_QUEUED_DISPATCHES);
        Self {
            dispatch_tx,
            dispatch_rx,
            cells: Arc::new(Mutex::new(HashMap::new())),
            dequeue_owner: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        dispatch_gate(&self.cells, cell_id).send_replace(true);
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        close_cell(&self.cells, cell_id);
    }

    pub(super) fn record_continuation(
        &self,
        cell_id: &CellId,
        continuation: PendingOwnerDrainedContinuation,
    ) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cell) = cells.get_mut(cell_id) else {
            return;
        };
        let Some(identity) = continuation.receipt.runtime_identity() else {
            return;
        };
        if cell.pending_continuations.len() < MAX_PENDING_CONTINUATIONS_PER_CELL
            && !cell
                .pending_continuations
                .iter()
                .any(|pending| pending.receipt.runtime_identity().as_ref() == Some(&identity))
        {
            cell.pending_continuations.push(continuation);
        }
    }

    pub(super) fn continuation_snapshot(
        &self,
        cell_id: &CellId,
    ) -> Vec<PendingOwnerDrainedContinuation> {
        let cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cells
            .get(cell_id)
            .map(|cell| cell.pending_continuations.clone())
            .unwrap_or_default()
    }

    pub(super) fn acknowledge_continuations(
        &self,
        cell_id: &CellId,
        accepted: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        if accepted.is_empty() {
            return;
        }
        let identities = accepted
            .iter()
            .filter_map(
                codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt::runtime_identity,
            )
            .collect::<std::collections::HashSet<_>>();
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = cells.get_mut(cell_id).is_some_and(|cell| {
            cell.pending_continuations.retain(|pending| {
                pending
                    .receipt
                    .runtime_identity()
                    .is_none_or(|identity| !identities.contains(&identity))
            });
            cell.terminal && cell.pending_continuations.is_empty()
        });
        if remove {
            cells.remove(cell_id);
        }
    }

    #[cfg(test)]
    pub(super) fn has_waitable_cells(&self) -> bool {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|cell| !cell.terminal)
    }

    pub(super) fn start_turn_worker(
        &self,
        exec: ExecContext,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
        request_signals: crate::session::turn_execution::SamplingRequestSignalCollector,
    ) -> CodeModeDispatchWorker {
        // Replacement and dequeue share one synchronous fence. Tasks already
        // admitted by the old owner retain their context and effects; it can
        // never take another queued message after this handoff.
        let owner = Arc::new(());
        {
            let mut current = self.dequeue_owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cleanup_terminal_cells(&self.cells);
            *current = Some(Arc::clone(&owner));
        }
        let tool_runtime = ToolCallRuntime::new(Arc::clone(&exec.session), step_context, tracker)
            .with_sampling_request_signals(request_signals);
        let host = Arc::new(CoreTurnHost { exec, tool_runtime });
        let dispatch_rx = self.dispatch_rx.clone();
        let cells = Arc::clone(&self.cells);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let dequeue_owner = Arc::clone(&self.dequeue_owner);
        let worker_owner = Arc::clone(&owner);
        tokio::spawn(async move {
            loop {
                let receive = dispatch_rx.recv();
                tokio::pin!(receive);
                let receive = std::future::poll_fn(|cx| {
                    let current = dequeue_owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &worker_owner)) {
                        return std::task::Poll::Ready(None);
                    }
                    std::future::Future::poll(receive.as_mut(), cx).map(Result::ok)
                });
                let message = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    message = receive => message,
                };
                let Some(message) = message else {
                    break;
                };
                if message.cancelled() {
                    continue;
                }
                match message {
                    DispatchMessage::Notify {
                        call_id,
                        cell_id,
                        text,
                        cancellation_token,
                        response_tx,
                    } => {
                        let host = Arc::clone(&host);
                        let cells = Arc::clone(&cells);
                        tokio::spawn(async move {
                            let ready = wait_until_cell_ready_for_dispatch(
                                &cells,
                                &cell_id,
                                &cancellation_token,
                            )
                            .await;
                            let response = if ready {
                                tokio::select! {
                                    biased;
                                    _ = cancellation_token.cancelled() => {
                                        Err("code mode notification cancelled".to_string())
                                    }
                                    response = host.notify(call_id, cell_id.clone(), text) => response,
                                }
                            } else {
                                close_cell(&cells, &cell_id);
                                Err("code mode notification cancelled".to_string())
                            };
                            let _ = response_tx.send(response);
                        });
                    }
                    DispatchMessage::InvokeTool {
                        invocation,
                        cancellation,
                        enqueued_at,
                        response_tx,
                    } => {
                        let host = Arc::clone(&host);
                        let cells = Arc::clone(&cells);
                        tokio::spawn(async move {
                            let cancellation_token = cancellation.token().clone();
                            let dequeued_at = Instant::now();
                            let cell_id = invocation.cell_id.clone();
                            let runtime_tool_call_id = invocation.runtime_tool_call_id.clone();
                            let tool_name = invocation.tool_name.clone();
                            info!(
                                event.name = "codex.code_mode_nested_tool.dispatch",
                                turn_id = %host.exec.turn.sub_id,
                                runtime_cell_id = %cell_id,
                                runtime_tool_call_id = %runtime_tool_call_id,
                                tool_name = %tool_name,
                                dispatch_queue_ms = duration_ms(
                                    dequeued_at.saturating_duration_since(enqueued_at),
                                ),
                                "code mode nested tool dispatched"
                            );
                            let ready = wait_until_cell_ready_for_dispatch(
                                &cells,
                                &cell_id,
                                &cancellation_token,
                            )
                            .await;
                            if !ready {
                                close_cell(&cells, &cell_id);
                                let _ = response_tx
                                    .send(Err("code mode nested tool call cancelled".to_string()));
                                return;
                            }
                            let child_started_at = Instant::now();
                            info!(
                                event.name = "codex.code_mode_nested_tool.child_start",
                                turn_id = %host.exec.turn.sub_id,
                                runtime_cell_id = %cell_id,
                                runtime_tool_call_id = %runtime_tool_call_id,
                                tool_name = %tool_name,
                                dispatch_gate_ms = duration_ms(
                                    child_started_at.saturating_duration_since(dequeued_at),
                                ),
                                "code mode nested tool child started"
                            );
                            let invocation = host.invoke_tool(invocation, cancellation.clone());
                            tokio::pin!(invocation);
                            let response = tokio::select! {
                                biased;
                                response = &mut invocation => response,
                                _ = cancellation_token.cancelled() => {
                                    // Keep polling the same owner future so
                                    // ToolCallRuntime can finish bounded
                                    // process/runtime cleanup before this
                                    // dispatch task releases its handles.
                                    let _ = invocation.await;
                                    Err("code mode nested tool call cancelled".to_string())
                                }
                            };
                            let child_completed_at = Instant::now();
                            let status = if response.is_ok() {
                                "completed"
                            } else {
                                "failed"
                            };
                            info!(
                                event.name = "codex.code_mode_nested_tool.child_end",
                                turn_id = %host.exec.turn.sub_id,
                                runtime_cell_id = %cell_id,
                                runtime_tool_call_id = %runtime_tool_call_id,
                                tool_name = %tool_name,
                                status,
                                child_runtime_ms = duration_ms(
                                    child_completed_at.saturating_duration_since(child_started_at),
                                ),
                                "code mode nested tool child ended"
                            );
                            let delivery_started_at = Instant::now();
                            let response_delivered = response_tx.send(response).is_ok();
                            let delivered_at = Instant::now();
                            info!(
                                event.name = "codex.code_mode_nested_tool",
                                turn_id = %host.exec.turn.sub_id,
                                runtime_cell_id = %cell_id,
                                runtime_tool_call_id = %runtime_tool_call_id,
                                tool_name = %tool_name,
                                status,
                                dispatch_queue_ms = duration_ms(
                                    dequeued_at.saturating_duration_since(enqueued_at),
                                ),
                                dispatch_gate_ms = duration_ms(
                                    child_started_at.saturating_duration_since(dequeued_at),
                                ),
                                child_runtime_ms = duration_ms(
                                    child_completed_at.saturating_duration_since(child_started_at),
                                ),
                                wrapper_delivery_ms = duration_ms(
                                    delivered_at.saturating_duration_since(delivery_started_at),
                                ),
                                total_ms = duration_ms(
                                    delivered_at.saturating_duration_since(enqueued_at),
                                ),
                                response_delivered,
                                "code mode nested tool completed"
                            );
                        });
                    }
                }
            }
            let current = dequeue_owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &worker_owner)) {
                cleanup_terminal_cells(&cells);
            }
        });
        CodeModeDispatchWorker {
            shutdown_tx: Some(shutdown_tx),
            dequeue_owner: Arc::clone(&self.dequeue_owner),
            owner,
        }
    }
}

fn dispatch_gate(
    cells: &Mutex<HashMap<CellId, CellDispatchState>>,
    cell_id: &CellId,
) -> watch::Sender<bool> {
    let mut cells = match cells.lock() {
        Ok(cells) => cells,
        Err(poisoned) => poisoned.into_inner(),
    };
    cells
        .entry(cell_id.clone())
        .or_insert_with(|| CellDispatchState {
            ready: watch::channel(false).0,
            terminal: false,
            pending_continuations: Vec::new(),
        })
        .ready
        .clone()
}

fn close_cell(cells: &Mutex<HashMap<CellId, CellDispatchState>>, cell_id: &CellId) {
    let mut cells = match cells.lock() {
        Ok(cells) => cells,
        Err(poisoned) => poisoned.into_inner(),
    };
    let remove = cells.get_mut(cell_id).is_some_and(|cell| {
        cell.terminal = true;
        cell.pending_continuations.is_empty()
    });
    if remove {
        cells.remove(cell_id);
    }
}

fn cleanup_terminal_cells(cells: &Mutex<HashMap<CellId, CellDispatchState>>) {
    cells
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|_, cell| !cell.terminal);
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn wait_until_cell_ready_for_dispatch(
    cells: &Mutex<HashMap<CellId, CellDispatchState>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let mut ready_rx = dispatch_gate(cells, cell_id).subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation: NestedCancellation,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let cancellation_token = cancellation.token().clone();
            let (response_tx, response_rx) = oneshot::channel();
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err("code mode nested tool call cancelled".to_string()),
                sent = self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation: cancellation.clone(),
                    enqueued_at: Instant::now(),
                    response_tx,
                }) => sent.map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?,
            }
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err("code mode notification cancelled".to_string()),
                sent = self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                }) => sent.map_err(|_| "code mode notification dispatcher is unavailable".to_string())?,
            }
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.close_cell(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation: NestedCancellation,
        enqueued_at: Instant,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

pub(crate) struct CodeModeDispatchWorker {
    shutdown_tx: Option<oneshot::Sender<()>>,
    dequeue_owner: Arc<Mutex<Option<Arc<()>>>>,
    owner: Arc<()>,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        let mut current = self.dequeue_owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &self.owner)) {
            *current = None;
        }
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

impl DispatchMessage {
    fn cancelled(&self) -> bool {
        match self {
            Self::InvokeTool { cancellation, response_tx, .. } => cancellation.is_cancelled() || response_tx.is_closed(),
            Self::Notify { cancellation_token, response_tx, .. } => cancellation_token.is_cancelled() || response_tx.is_closed(),
        }
    }
}

struct CoreTurnHost {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
}

impl CoreTurnHost {
    async fn invoke_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation: NestedCancellation,
    ) -> Result<JsonValue, String> {
        call_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn notify(&self, call_id: String, _cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_internal_no_new_turn(
                vec![ResponseItem::CustomToolCallOutput {
                    id: None,
                    call_id,
                    name: Some(PUBLIC_TOOL_NAME.to_string()),
                    output: FunctionCallOutputPayload::from_text(text),
                    internal_chat_message_metadata_passthrough: None,
                }],
                Some(&self.exec.turn),
            )
            .await
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_callbacks_cannot_grow_the_retained_dispatch_queue() {
        let broker = CodeModeDispatchBroker::new();
        for index in 0..MAX_QUEUED_DISPATCHES * 4 {
            let cancellation = CancellationToken::new();
            let notify = broker.notify("parent".into(), CellId::new("cell".into()), "payload".repeat(1000), cancellation.clone());
            tokio::pin!(notify);
            assert!(futures::poll!(&mut notify).is_pending());
            cancellation.cancel();
            assert!(notify.await.is_err());
            assert_eq!(broker.dispatch_rx.len(), (index + 1).min(MAX_QUEUED_DISPATCHES));
        }
        while let Ok(message) = broker.dispatch_rx.try_recv() {
            assert!(message.cancelled(), "retired entries must never dispatch");
        }
    }

    #[tokio::test]
    async fn replacement_fences_retired_worker_before_queued_notification() -> anyhow::Result<()> {
        let (old_session, old_turn) = crate::session::tests::make_session_and_context().await;
        let (new_session, new_turn) = crate::session::tests::make_session_and_context().await;
        let old_session = Arc::new(old_session);
        let new_session = Arc::new(new_session);
        let broker = CodeModeDispatchBroker::new();
        let start = |session: Arc<crate::session::Session>, turn| {
            let turn = Arc::new(turn);
            broker.start_turn_worker(ExecContext { session, turn: Arc::clone(&turn) }, StepContext::for_test(turn),
                Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())), Default::default())
        };
        let old = start(Arc::clone(&old_session), old_turn);
        let replacement = start(Arc::clone(&new_session), new_turn);
        drop(old); // A late old drop must not revoke its replacement.
        let cell = CellId::new("handoff".into());
        broker.mark_cell_ready_for_dispatch(&cell);
        tokio::time::timeout(std::time::Duration::from_secs(10), broker.notify("handoff-parent".into(), cell, "replacement-only".into(), CancellationToken::new())).await?.map_err(anyhow::Error::msg)?;
        let old_history = serde_json::to_string(&old_session.clone_history().await.into_raw_items())?;
        let new_history = serde_json::to_string(&new_session.clone_history().await.into_raw_items())?;
        assert!(!old_history.contains("replacement-only"));
        assert!(new_history.contains("replacement-only"));
        drop(replacement);
        Ok(())
    }

    use codex_protocol::protocol::DeterministicContinuationClass;
    use codex_protocol::protocol::DeterministicContinuationHostAction;
    use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
    use serde_json::json;

    fn continuation(ordinal: usize) -> PendingOwnerDrainedContinuation {
        PendingOwnerDrainedContinuation {
            preserved_content: vec![json!({"ordinal": ordinal})],
            receipt: TurnTimingDeterministicContinuationReceipt {
                class: DeterministicContinuationClass::ArtifactRange,
                wire_identity: String::new(),
                resource_identity_hash: format!("artifact-{ordinal}"),
                state_revision: "revision".to_string(),
                host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
                action_bounds_hash: "test-bounds".to_string(),
                suppressed_continuation_count: 1,
            },
        }
    }

    #[tokio::test]
    async fn cancellation_returns_buffered_cell_output_and_nested_receipts() -> anyhow::Result<()> {
        use crate::tools::code_mode::{CodeModeService, CodeModeNestedResultEvidence};
        use crate::tools::code_mode::execute_handler::CodeModeExecuteHandler;
        use crate::tools::code_mode::wait_handler::CodeModeWaitHandler;
        use crate::tools::context::ToolPayload;
        use crate::tools::registry::{CoreToolRuntime, ToolRegistry};
        use crate::tools::router::{ToolCall, ToolRouter};
        use codex_tools::ToolName;
        for use_wait in [false, true] {
            let (mut session, turn) = crate::session::tests::make_session_and_context().await;
            session.services.code_mode_service = CodeModeService::new(Arc::new(codex_code_mode::InProcessCodeModeSessionProvider));
            let session = Arc::new(session);
            let step = StepContext::for_test(Arc::new(turn));
            let execute = CodeModeExecuteHandler::new(
                crate::tools::code_mode::execute_spec::create_code_mode_tool(true, false, &[], &[]), Vec::new(), Vec::new(),
            ).map_err(anyhow::Error::msg)?;
            let handlers: Vec<Arc<dyn CoreToolRuntime>> = vec![Arc::new(execute), Arc::new(CodeModeWaitHandler)];
            let router = Arc::new(ToolRouter::from_parts(ToolRegistry::from_tools(handlers), Vec::new()));
            let step = step.with_tool_router_for_test(router);
            let timing = Arc::clone(&step.turn.turn_timing_state);
            let tracker = Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new()));
            let runtime = ToolCallRuntime::new(Arc::clone(&session), step, tracker);
            let service = &session.services.code_mode_service;
            // Notification admission is a deterministic rendezvous after text(),
            // not a sleep or a second observer that drains the buffered output.
            let source = "text('buffered before interrupt'); await notify('ready');";
            let call = if use_wait {
                let started = service.execute(codex_code_mode::ExecuteRequest {
                    state_path: None, tool_call_id: "outer".into(), enabled_tools: Vec::new().into(),
                    source: format!("await yield_control(); {source}"), yield_time_ms: None,
                    max_output_tokens: None, default_tool_timeout_ms: None,
                }).await.map_err(anyhow::Error::msg)?;
                let cell = started.cell_id.clone();
                service.record_cell_parent_call_id(&cell, "outer");
                service.mark_cell_ready_for_dispatch(&cell);
                assert!(matches!(started.initial_response().await.unwrap(), codex_code_mode::RuntimeResponse::ExplicitYield { .. }));
                ToolCall { tool_name: ToolName::plain("wait"), call_id: "wait-call".into(),
                    payload: ToolPayload::Function { arguments: json!({"cell_id": cell.as_str()}).to_string() } }
            } else {
                ToolCall { tool_name: ToolName::plain("exec"), call_id: "outer".into(),
                    payload: ToolPayload::Custom { input: source.into() } }
            };
            let cancellation = CancellationToken::new();
            let task = tokio::spawn(runtime.handle_tool_call(call, cancellation.clone()));
            let notification = tokio::time::timeout(std::time::Duration::from_secs(10), service.dispatch_broker.dispatch_rx.recv()).await??;
            let DispatchMessage::Notify { cell_id, .. } = &notification else { panic!("expected notification rendezvous"); };
            // A yielded cell can enqueue its notification before wait is admitted.
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while timing.lifecycle_context().active_tool_count == 0 {
                    tokio::task::yield_now().await;
                }
            }).await?;
            let ordinal = service.begin_packet_call(cell_id).unwrap();
            service.complete_packet_call(cell_id, ordinal, false, 0, Vec::new(), Some(CodeModeNestedResultEvidence {
                failed: false, ordinal, call_id: "completed-nested".into(), parent_call_id: Some("outer".into()),
                parent_cell_id: cell_id.to_string(), runtime_tool_call_id: "nested-1".into(), tool_name: "exec_command".into(),
                output: json!({"output": "completed nested result", "session_id": 60964, "process_exited": false}).to_string(),
                output_truncated: false,
                output_fingerprints: Vec::new(),
                command_state: Some(json!({"tool":"exec_command", "session_id":60964, "process_exited":false})),
            }), None);
            cancellation.cancel();
            let response = tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
            let text = serde_json::to_string(&response)?;
            assert!(text.contains("buffered before interrupt"), "{text}");
            assert!(text.contains("completed nested result"), "{text}");
            assert!(text.contains("60964"), "{text}");
            assert!(!text.contains("cleanup_state"), "cleanup must finish before the response: {text}");
            assert!(!service.dispatch_broker.has_waitable_cells());
            drop(notification);
            service.shutdown().await.map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    #[test]
    fn cell_owner_continuations_are_bounded_and_acknowledged_by_receipt() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-a".to_string());
        broker.mark_cell_ready_for_dispatch(&cell_id);
        for ordinal in 0..=MAX_PENDING_CONTINUATIONS_PER_CELL {
            broker.record_continuation(&cell_id, continuation(ordinal));
        }

        broker.close_cell(&cell_id);
        assert!(!broker.has_waitable_cells());
        let snapshot = broker.continuation_snapshot(&cell_id);
        assert_eq!(snapshot.len(), MAX_PENDING_CONTINUATIONS_PER_CELL);
        assert_eq!(snapshot[0].preserved_content, vec![json!({"ordinal": 0})]);
        assert_eq!(
            snapshot[MAX_PENDING_CONTINUATIONS_PER_CELL - 1].preserved_content,
            vec![json!({"ordinal": MAX_PENDING_CONTINUATIONS_PER_CELL - 1})]
        );
        broker.acknowledge_continuations(&cell_id, &[snapshot[0].receipt.clone()]);
        assert_eq!(
            broker.continuation_snapshot(&cell_id).len(),
            MAX_PENDING_CONTINUATIONS_PER_CELL - 1
        );
        broker.acknowledge_continuations(
            &cell_id,
            &snapshot
                .into_iter()
                .skip(1)
                .map(|continuation| continuation.receipt)
                .collect::<Vec<_>>(),
        );
        assert!(broker.continuation_snapshot(&cell_id).is_empty());
    }

    #[test]
    fn continuation_snapshot_is_non_destructive_until_acknowledged() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-b".to_string());
        broker.mark_cell_ready_for_dispatch(&cell_id);
        broker.record_continuation(&cell_id, continuation(0));

        let snapshot = broker.continuation_snapshot(&cell_id);
        assert_eq!(snapshot.len(), 1);
        let repeated = broker.continuation_snapshot(&cell_id);
        assert_eq!(repeated.len(), 1);
        assert_eq!(
            repeated[0].receipt.runtime_identity(),
            snapshot[0].receipt.runtime_identity()
        );
        assert!(broker.has_waitable_cells());
        broker.acknowledge_continuations(&cell_id, &[snapshot[0].receipt.clone()]);
        assert!(broker.continuation_snapshot(&cell_id).is_empty());
        broker.close_cell(&cell_id);
        assert!(!broker.has_waitable_cells());
    }

    #[test]
    fn wire_only_receipt_cannot_acknowledge_bounds_sensitive_continuation() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-wire-only".to_string());
        broker.mark_cell_ready_for_dispatch(&cell_id);
        broker.record_continuation(&cell_id, continuation(0));

        let authoritative = broker.continuation_snapshot(&cell_id);
        let wire = serde_json::to_value(&authoritative[0].receipt).expect("serialize receipt");
        let wire_only: TurnTimingDeterministicContinuationReceipt =
            serde_json::from_value(wire).expect("deserialize validated public receipt");
        assert!(wire_only.runtime_identity().is_none());

        broker.acknowledge_continuations(&cell_id, &[wire_only]);
        assert_eq!(broker.continuation_snapshot(&cell_id).len(), 1);
    }

    #[test]
    fn duplicate_receipt_is_recorded_only_once() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-c".to_string());
        broker.mark_cell_ready_for_dispatch(&cell_id);
        let first = continuation(0);
        let duplicate = PendingOwnerDrainedContinuation {
            preserved_content: vec![json!({"ordinal": "duplicate"})],
            receipt: first.receipt.clone(),
        };

        broker.record_continuation(&cell_id, first);
        broker.record_continuation(&cell_id, duplicate);

        let snapshot = broker.continuation_snapshot(&cell_id);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].preserved_content, vec![json!({"ordinal": 0})]);
    }

    #[test]
    fn worker_cleanup_removes_terminal_cells_and_preserves_live_cells() {
        let broker = CodeModeDispatchBroker::new();
        let cell_id = CellId::new("cell-d".to_string());
        broker.mark_cell_ready_for_dispatch(&cell_id);
        broker.record_continuation(&cell_id, continuation(0));
        broker.close_cell(&cell_id);
        assert_eq!(broker.continuation_snapshot(&cell_id).len(), 1);
        let first = CellId::new("cell-live".to_string());
        let second = CellId::new("cell-pending".to_string());
        broker.mark_cell_ready_for_dispatch(&first);
        broker.mark_cell_ready_for_dispatch(&second);
        broker.record_continuation(&second, continuation(0));
        assert!(broker.has_waitable_cells());

        cleanup_terminal_cells(&broker.cells);

        assert!(broker.continuation_snapshot(&cell_id).is_empty());
        let cells = broker.cells.lock().unwrap();
        assert!(!cells.contains_key(&cell_id));
        assert!(cells.contains_key(&first));
        assert!(cells.contains_key(&second));
        drop(cells);
        assert!(broker.has_waitable_cells());
        assert!(broker.continuation_snapshot(&first).is_empty());
        assert_eq!(broker.continuation_snapshot(&second).len(), 1);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Hold live history to verify durable notification acceptance survives cancellation before live publication"
    )]
    async fn broker_notify_cancellation_preserves_physical_live_and_model_history()
    -> anyhow::Result<()> {
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::protocol::RolloutItem;
        use std::time::Duration;

        fn outputs(items: &[ResponseItem], call: &str) -> Vec<ResponseItem> {
            items
                .iter()
                .filter(|item| {
                    matches!(item,
                ResponseItem::CustomToolCallOutput { call_id, .. } if call_id == call)
                })
                .cloned()
                .collect()
        }
        async fn physical(path: &std::path::Path) -> anyhow::Result<Vec<ResponseItem>> {
            let history =
                crate::rollout::recorder::RolloutRecorder::get_rollout_history(path).await?;
            Ok(history
                .get_rollout_items()
                .iter()
                .filter_map(|item| match item {
                    RolloutItem::ResponseItem(item) => Some(item.clone()),
                    _ => None,
                })
                .collect())
        }
        const CALL: &str = "broker-notify-parent";
        const TEXT: &str = "accepted notification evidence: 7 * 6 = 42";
        let home = tempfile::tempdir()?;
        let (mut session, turn, events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |_| {},
            )
            .await;
        let rollout = crate::session::tests::attach_thread_persistence(
            Arc::get_mut(&mut session).expect("unique fixture session"),
        )
        .await;
        let call = ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: CALL.to_string(),
            name: PUBLIC_TOOL_NAME.to_string(),
            namespace: None,
            input: "notify('accepted notification evidence: 7 * 6 = 42')".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items_ordered(&turn, &[call])
            .await?;
        session.flush_rollout_after_ordered_commits(&turn).await?;
        session.request_raw_response_items();
        assert!(
            session.active_turn.lock().await.is_none(),
            "exercise idle fallback without starting a turn"
        );

        let broker = Arc::new(CodeModeDispatchBroker::new());
        let cell = CellId::new("normal-broker-notify-cell".to_string());
        broker.mark_cell_ready_for_dispatch(&cell);
        let worker = broker.start_turn_worker(
            ExecContext {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn),
            },
            StepContext::for_test(Arc::clone(&turn)),
            Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            Default::default(),
        );
        let canceled = CancellationToken::new();
        canceled.cancel();
        assert!(
            broker
                .notify(
                    "before-admission".to_string(),
                    cell.clone(),
                    "forbidden".to_string(),
                    canceled
                )
                .await
                .is_err()
        );
        assert!(
            broker.dispatch_rx.is_empty(),
            "already-cancelled notification never enters broker queue"
        );

        // Hold the real live-history mutex while the normal broker worker appends
        // the notification through the real LocalThreadStore/physical rollout.
        let state = session.lock_history_state_for_test().await;
        let cancel = CancellationToken::new();
        let notify = {
            let broker = Arc::clone(&broker);
            let cell = cell.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                broker
                    .notify(CALL.to_string(), cell, TEXT.to_string(), cancel)
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // The fixture deliberately holds session state; only drain the
                // physical recorder, without reacquiring it for artifact sync.
                session.live_thread().expect("attached persistence").flush().await?;
                if outputs(&physical(&rollout).await?, CALL).len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        assert!(
            outputs(&state.clone_history().into_raw_items(), CALL).is_empty(),
            "physical acceptance precedes blocked live history"
        );
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), notify)
            .await??
            .expect_err("delegate reports cancelled waiter");
        assert_eq!(error, "code mode notification cancelled");
        drop(state);
        tokio::time::timeout(
            Duration::from_secs(5),
            session.flush_rollout_after_ordered_commits(&turn),
        )
        .await??;

        let saved = outputs(&physical(&rollout).await?, CALL);
        let live = outputs(&session.clone_history().await.into_raw_items(), CALL);
        assert_eq!(
            saved.len(),
            1,
            "accepted output appears exactly once physically"
        );
        assert_eq!(
            live, saved,
            "cancelled waiter cannot split physical and live history"
        );
        let model = outputs(
            &session
                .clone_history()
                .await
                .for_prompt(&turn.model_info.input_modalities),
            CALL,
        );
        assert_eq!(
            model, saved,
            "normal next-prompt projection retains the accepted output"
        );
        let ResponseItem::CustomToolCallOutput { output, .. } = &saved[0] else {
            unreachable!()
        };
        assert_eq!(
            output,
            &FunctionCallOutputPayload::from_text(TEXT.to_string())
        );

        // A new emission with identical text is not a retry of the cancelled wait.
        broker
            .notify(
                CALL.to_string(),
                cell.clone(),
                TEXT.to_string(),
                CancellationToken::new(),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        session.flush_rollout_after_ordered_commits(&turn).await?;
        let repeated = outputs(&physical(&rollout).await?, CALL);
        assert_eq!(
            repeated.len(),
            2,
            "intentional repeated notifications remain distinct"
        );
        assert_ne!(repeated[0].id(), repeated[1].id());
        assert_eq!(
            outputs(&session.clone_history().await.into_raw_items(), CALL),
            repeated
        );
        assert!(outputs(&physical(&rollout).await?, "before-admission").is_empty());
        assert!(
            session.active_turn.lock().await.is_none(),
            "notification never starts a turn"
        );
        assert!(
            !session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await,
            "internal notification never becomes user steering"
        );
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::RawResponseItem(event) => published.push(event.item),
                EventMsg::TurnStarted(_) => panic!("notification started a model turn"),
                _ => {}
            }
        }
        assert_eq!(
            outputs(&published, CALL),
            repeated,
            "raw subscribers receive both committed outputs exactly once"
        );
        assert!(outputs(&published, "before-admission").is_empty());
        broker.close_cell(&cell);
        drop(worker);
        Ok(())
    }
}
