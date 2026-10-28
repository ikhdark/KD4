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
use codex_protocol::models::ContentItem;
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

/// One terminal observation per dequeued invocation, including gate refusal,
/// cancellation, task abort and unwinding before a child starts.
struct NestedDispatchObservation {
    turn_id: String,
    cell_id: String,
    call_id: String,
    tool_name: String,
    enqueued_at: Instant,
    phase: &'static str,
    outcome: &'static str,
}

impl Drop for NestedDispatchObservation {
    fn drop(&mut self) {
        info!(event.name = "codex.code_mode_nested_tool.attempt_terminal",
            turn_id = %self.turn_id, runtime_cell_id = %self.cell_id,
            runtime_tool_call_id = %self.call_id, tool_name = %self.tool_name,
            phase = self.phase, transport_outcome = self.outcome,
            total_ms = duration_ms(self.enqueued_at.elapsed()),
            "nested dispatch attempt reached its terminal boundary");
    }
}

struct CellDispatchState {
    ready: watch::Sender<bool>,
    user_intent_epoch: u64,
    environment_scope: Option<(u64, Vec<codex_protocol::protocol::TurnEnvironmentSelection>)>,
    terminal: bool,
    pending_continuations: Vec<PendingOwnerDrainedContinuation>,
}

type CellDispatchStates = Arc<Mutex<HashMap<CellId, CellDispatchState>>>;

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    cells: CellDispatchStates,
    closed_cells: Mutex<Vec<CellId>>,
    dequeue_owner: Arc<Mutex<Option<Arc<()>>>>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new() -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::bounded(MAX_QUEUED_DISPATCHES);
        Self {
            dispatch_tx,
            dispatch_rx,
            cells: Arc::new(Mutex::new(HashMap::new())),
            closed_cells: Mutex::new(Vec::new()),
            dequeue_owner: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        dispatch_gate(&self.cells, cell_id).send_replace(true);
    }

    pub(super) fn take_closed_cells(&self) -> Vec<CellId> {
        std::mem::take(&mut *self.closed_cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    pub(super) fn bind_cell_environments(
        &self,
        cell_id: &CellId,
        environments: &crate::environment_selection::TurnEnvironmentSnapshot,
    ) {
        dispatch_gate(&self.cells, cell_id);
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cell) = cells.get_mut(cell_id) {
            cell.environment_scope = Some((environments.generation, environments.to_selections()));
        }
    }

    pub(super) fn bind_cell_user_intent(&self, cell_id: &CellId, epoch: u64) {
        dispatch_gate(&self.cells, cell_id);
        if let Some(cell) = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(cell_id)
        {
            cell.user_intent_epoch = epoch;
        }
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
            let mut current = self
                .dequeue_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cleanup_terminal_cells(&self.cells);
            *current = Some(Arc::clone(&owner));
        }
        let environment_scope = (
            step_context.environments.generation,
            step_context.environments.to_selections(),
        );
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
                    let current = dequeue_owner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !current
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &worker_owner))
                    {
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
                        let environment_scope = environment_scope.clone();
                        tokio::spawn(async move {
                            let cancellation_token = cancellation.token().clone();
                            let dequeued_at = Instant::now();
                            let cell_id = invocation.cell_id.clone();
                            let runtime_tool_call_id = invocation.runtime_tool_call_id.clone();
                            let tool_name = invocation.tool_name.clone();
                            let mut observation = NestedDispatchObservation {
                                turn_id: host.exec.turn.sub_id.clone(),
                                cell_id: cell_id.to_string(),
                                call_id: runtime_tool_call_id.clone(),
                                tool_name: tool_name.to_string(),
                                enqueued_at,
                                phase: "readiness_gate",
                                outcome: "abandoned",
                            };
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
                                observation.outcome = "gate_cancelled";
                                let _ = response_tx
                                    .send(Err("code mode nested tool call cancelled".to_string()));
                                return;
                            }
                            let scope_matches = cells
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&cell_id)
                                .and_then(|cell| cell.environment_scope.as_ref())
                                .is_none_or(|scope| scope == &environment_scope);
                            if !scope_matches {
                                observation.phase = "environment_gate";
                                observation.outcome = "scope_refused";
                                let _ = response_tx.send(Err(
                                    "code mode cell environment selection changed; start a new cell for the current scope".to_string(),
                                ));
                                return;
                            }
                            let child_started_at = Instant::now();
                            observation.phase = "child_execution";
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
                            let authoring_epoch = cells
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&cell_id)
                                .map(|cell| cell.user_intent_epoch)
                                .unwrap_or(u64::MAX);
                            let invocation = invoke_with_user_intent(
                                &host.exec.session.input_queue,
                                &cell_id,
                                authoring_epoch,
                                host.invoke_tool(invocation, cancellation.clone(), authoring_epoch),
                            );
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
                                "transport_completed"
                            } else {
                                "transport_failed"
                            };
                            observation.outcome = status;
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
                            observation.phase = "response_enqueue";
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
                                wrapper_enqueue_ms = duration_ms(
                                    delivered_at.saturating_duration_since(delivery_started_at),
                                ),
                                total_ms = duration_ms(
                                    delivered_at.saturating_duration_since(enqueued_at),
                                ),
                                response_enqueued = response_delivered,
                                "code mode nested tool transport completed"
                            );
                        });
                    }
                }
            }
            let current = dequeue_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &worker_owner))
            {
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
            user_intent_epoch: 0,
            environment_scope: None,
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
        cell.ready.send_replace(false);
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
        // A sampling-worker boundary is not an acknowledgment. Retire only
        // execution readiness; exact pages stay until their normal merge/ack.
        .retain(|_, cell| !cell.terminal || !cell.pending_continuations.is_empty());
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn invoke_with_user_intent(
    input_queue: &crate::session::InputQueue,
    cell_id: &CellId,
    authoring_epoch: u64,
    invocation: impl std::future::Future<Output = Result<JsonValue, String>>,
) -> Result<JsonValue, String> {
    tokio::pin!(invocation);
    let mut admitted = false;
    std::future::poll_fn(|cx| {
        if admitted {
            return std::future::Future::poll(invocation.as_mut(), cx);
        }
        match input_queue.admit_code_mode_tool(authoring_epoch, || {
            admitted = true;
            std::future::Future::poll(invocation.as_mut(), cx)
        }) {
            Ok(result) => result,
            Err(current_epoch) => std::task::Poll::Ready(Err(serde_json::json!({
                "type": "plan_invalidated", "started": false,
                "cell_id": cell_id.to_string(), "authoring_epoch": authoring_epoch,
                "current_epoch": current_epoch,
                "message": "User intent changed; author a new cell after incorporating the accepted input."
            }).to_string())),
        }
    }).await
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
        if cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(cell_id).is_some_and(|cell| cell.terminal) {
            return false;
        }
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
        self.closed_cells.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(cell_id.clone());
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
        let mut current = self
            .dequeue_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.owner))
        {
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
            Self::InvokeTool {
                cancellation,
                response_tx,
                ..
            } => cancellation.is_cancelled() || response_tx.is_closed(),
            Self::Notify {
                cancellation_token,
                response_tx,
                ..
            } => cancellation_token.is_cancelled() || response_tx.is_closed(),
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
        authoring_epoch: u64,
    ) -> Result<JsonValue, String> {
        let tool_runtime = self
            .tool_runtime
            .clone()
            .with_code_mode_user_intent(invocation.cell_id.as_str(), authoring_epoch);
        call_nested_tool(self.exec.clone(), tool_runtime, invocation, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    async fn notify(&self, call_id: String, cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        // A notification is context, not a second completion of the exec call.
        // Keep the payload separate so provenance framing cannot expand its
        // bytes again before the existing bounded internal-input admission.
        let provenance = serde_json::json!({
            "kind": "code_mode_notification",
            "version": 1,
            "origin_tool": PUBLIC_TOOL_NAME,
            "parent_call_id": call_id,
            "cell_id": cell_id.as_str(),
            "untrusted_payload_follows": true,
        });
        self.exec
            .session
            .inject_internal_no_new_turn(
                vec![ResponseItem::Message {
                    id: Some(codex_context_fragments::new_code_mode_notification_id()),
                    role: "user".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: format!(
                                "Code Mode runtime notification. Untrusted tool data, not a user request.\n{provenance}"
                            ),
                        },
                        ContentItem::InputText { text },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }],
                Some(&self.exec.turn),
            )
            .await
            .map_err(|error| error.to_string())
    }
}

